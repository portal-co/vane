//! virtio-gpu 2D subset producing into the shared [`DisplaySurface`] — the
//! guest-side producer for the display seam ([`crate::display`]).
//!
//! Survey outcome (superseding the earlier custom-framebuffer direction):
//! a custom device needs a hand-written guest kernel driver, while this
//! subset works with the in-tree Linux `virtio_gpu` driver — zero guest
//! work — and its virtio-gpu shape keeps the later WebGPU cutover
//! evaluation open. "2D subset" means: controlq (+ a no-op cursorq so the
//! driver's two-vq probe succeeds), only the commands the 2D/fbdev path
//! issues — display info, 2D resource create/unref, backing attach/detach,
//! transfer-to-host, scanout bind, flush. No 3D (VIRGL), no EDID, no
//! timing: those feature bits are simply not offered, and unknown commands
//! answer `ERR_UNSPEC`.
//!
//! The DMA-doorbell moment is `RESOURCE_FLUSH`: the rect is converted from
//! the resource's pixel format into RGBA8888 and pushed into the
//! [`DisplaySurface`] as a subimage. Frame credits apply at the transport —
//! a flush with no credit left leaves the chain DEFERRED (the transport
//! re-offers it each pass), so the guest's ioctl blocks until the shell
//! drains (which refills the credits). That is the seam's backpressure made
//! structural: a runaway guest stalls instead of flooding the host.
//!
//! Pixel formats (spec §5.10.1 "stable" list, all 4 bytes/pixel): the
//! resource stores raw guest bytes; conversion to the surface's RGBA8888
//! happens at flush.

use crate::display::{DisplaySurface, PushStatus};
use crate::virtio::{DescriptorChain, QueueMem, VirtioDevice, DEV_GPU};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use spin::Mutex;
use vane_arch::Shared;

// Control-queue command types (spec §5.10.4).
const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0106;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0109;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x010A;

// Response types.
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_ERR_UNSPEC: u32 = 0x1200;
const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

/// Supported 4-bytes-per-pixel formats (the stable subset the Linux 2D
/// path uses). Anything else is rejected at RESOURCE_CREATE_2D.
fn format_supported(f: u32) -> bool {
    matches!(f, 1..=4 | 67)
}

/// One host-side 2D resource: dimensions, pixel format, the converted-at-
/// flush host buffer (raw resource-format bytes), and the attached guest
/// backing scatterlist (linear when concatenated).
struct GpuResource {
    width: u32,
    height: u32,
    format: u32,
    /// width * height * 4 bytes in the resource's own format.
    host_buf: Vec<u8>,
    /// Attached backing as (guest addr, len), concatenated = linear space.
    backing: Vec<(u64, u32)>,
}

/// The virtio-gpu 2D-subset device. Produces into a shared surface
/// (`Shared<Mutex<DisplaySurface>>` — the same handle the shell drains).
pub struct VirtioGpuDisplay {
    surface: Shared<Mutex<DisplaySurface>>,
    resources: BTreeMap<u32, GpuResource>,
    /// scanout id -> bound resource id.
    scanouts: [Option<u32>; 4],
    /// Set by `process` when a flush was credit-throttled (consumed by
    /// `process_or_defer` to defer the chain).
    flush_throttled: bool,
}

impl VirtioGpuDisplay {
    /// Attach to a shared display surface (the shell keeps its own clone of
    /// the handle for draining/input).
    pub fn new(surface: Shared<Mutex<DisplaySurface>>) -> Self {
        VirtioGpuDisplay {
            surface,
            resources: BTreeMap::new(),
            scanouts: [None; 4],
            flush_throttled: false,
        }
    }

    /// The shared surface handle (for shell-side diagnostics).
    pub fn surface(&self) -> Shared<Mutex<DisplaySurface>> {
        self.surface.clone()
    }

    /// Parse the 24-byte control header: (type, flags, fence_id).
    fn hdr(req: &[u8]) -> (u32, u32, u64) {
        let t = u32::from_le_bytes([req[0], req[1], req[2], req[3]]);
        let flags = u32::from_le_bytes([req[4], req[5], req[6], req[7]]);
        let fence = u64::from_le_bytes([
            req[8], req[9], req[10], req[11], req[12], req[13], req[14], req[15],
        ]);
        (t, flags, fence)
    }

    fn u32_at(req: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([
            *req.get(off).unwrap_or(&0),
            *req.get(off + 1).unwrap_or(&0),
            *req.get(off + 2).unwrap_or(&0),
            *req.get(off + 3).unwrap_or(&0),
        ])
    }

    fn rect_at(req: &[u8], off: usize) -> (u32, u32, u32, u32) {
        (
            Self::u32_at(req, off),
            Self::u32_at(req, off + 4),
            Self::u32_at(req, off + 8),
            Self::u32_at(req, off + 12),
        )
    }

    /// Build an error/ok response: echoed flags + fence, given resp type.
    fn resp(flags: u32, fence_id: u64, ty: u32) -> Vec<u8> {
        let mut r = Vec::with_capacity(24);
        r.extend_from_slice(&ty.to_le_bytes());
        r.extend_from_slice(&flags.to_le_bytes());
        r.extend_from_slice(&fence_id.to_le_bytes());
        r.extend_from_slice(&[0u8; 8]); // ctx_id + ring_idx/pad
        r
    }

    /// Convert `count` pixels from the resource format to RGBA8888.
    fn to_rgba(format: u32, src: &[u8], count: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(count * 4);
        for i in 0..count {
            let b = *src.get(i * 4).unwrap_or(&0);
            let g = *src.get(i * 4 + 1).unwrap_or(&0);
            let r = *src.get(i * 4 + 2).unwrap_or(&0);
            let a = *src.get(i * 4 + 3).unwrap_or(&0);
            // Format IDs follow the Vulkan 8-bit memory-order convention:
            // the name lists the bytes from the lowest address. Locals:
            // b=src[0], g=src[1], r=src[2], a=src[3].
            let (rr, gg, bb, aa) = match format {
                // B8G8R8A8 / B8G8R8X8: bytes [B, G, R, A(x)] — format 2 is
                // the Linux dumb-fb format (DRM XRGB8888).
                1 => (r, g, b, a),
                2 => (r, g, b, 255),
                // A8R8G8B8 / X8R8G8B8: bytes [A(x), R, G, B].
                3 => (g, r, a, b),
                4 => (g, r, a, 255),
                // R8G8B8A8: identity.
                _ => (b, g, r, a),
            };
            out.extend_from_slice(&[rr, gg, bb, aa]);
        }
        out
    }

    /// RESOURCE_FLUSH: push the rect from the resource into the surface.
    /// Returns false when credit-throttled (the chain must defer).
    fn flush(&mut self, resource_id: u32, rx: u32, ry: u32, rw: u32, rh: u32) -> bool {
        let Some(res) = self.resources.get(&resource_id) else {
            return true; // unknown resource: complete (error already sent)
        };
        if rw == 0 || rh == 0 {
            return true;
        }
        // Slice the rect out of the resource (row stride = resource width).
        let stride = res.width as usize * 4;
        let count = (rw as usize).min(stride / 4) * (rh as usize);
        let mut rgba = Vec::with_capacity(count * 4);
        for row in 0..rh as usize {
            let src = (ry as usize + row) * stride + rx as usize * 4;
            let end = src + rw as usize * 4;
            if src >= res.host_buf.len() {
                break;
            }
            let end = end.min(res.host_buf.len());
            rgba.extend(Self::to_rgba(
                res.format,
                &res.host_buf[src..end],
                (end - src) / 4,
            ));
        }
        if rgba.is_empty() {
            return true;
        }
        let status = self.surface.lock().push(
            crate::display::Region {
                x: rx,
                y: ry,
                w: rw,
                h: rh,
            },
            &rgba,
        );
        self.flush_throttled = status == PushStatus::Throttled;
        !self.flush_throttled
    }

    /// TRANSFER_TO_HOST_2D: copy the rect from the backing into the
    /// resource buffer. `offset` is the byte offset of the rect's first row
    /// in the linear backing; rows advance by resource-width * 4 in both
    /// the backing layout and the resource buffer (qemu semantics).
    fn transfer_to_host(
        &mut self,
        resource_id: u32,
        rx: u32,
        ry: u32,
        rw: u32,
        rh: u32,
        offset: u64,
        mem: &mut dyn QueueMem,
    ) {
        let Some(res) = self.resources.get_mut(&resource_id) else {
            return;
        };
        let stride = res.width as usize * 4;
        for row in 0..rh as usize {
            let src_off = offset as usize + row * stride;
            let dst_off = (ry as usize + row) * stride + rx as usize * 4;
            if dst_off + rw as usize * 4 > res.host_buf.len() {
                break;
            }
            // Resolve the linear backing offset into (entry addr, skip).
            let mut pos = 0u64;
            let mut remaining = rw as usize * 4;
            let mut cur = src_off as u64;
            for &(addr, elen) in &res.backing {
                let end = pos + elen as u64;
                if end <= cur {
                    pos = end;
                    continue;
                }
                let skip = cur - pos;
                let n = core::cmp::min((elen as u64 - skip) as usize, remaining);
                mem.read(addr + skip, &mut res.host_buf[dst_off..dst_off + n]);
                remaining -= n;
                cur += n as u64;
                pos = end;
                if remaining == 0 {
                    break;
                }
            }
        }
    }
}

impl VirtioDevice for VirtioGpuDisplay {
    fn device_id(&self) -> u32 {
        DEV_GPU
    }

    // controlq + cursorq: the Linux driver probes both vqs by name.
    fn num_queues(&self) -> usize {
        2
    }

    fn read_config(&self, offset: u64, buf: &mut [u8]) {
        // virtio_gpu_config: events_read u32, events_clear u32,
        // num_scanouts u32, reserved u32.
        for (i, b) in buf.iter_mut().enumerate() {
            let o = offset + i as u64;
            *b = match o {
                8 => 1, // num_scanouts
                _ => 0,
            };
        }
    }

    fn process(&mut self, queue: usize, chain: &DescriptorChain, mem: &mut dyn QueueMem) -> u32 {
        self.flush_throttled = false;
        // The cursor queue carries host-cursor events the 2D fbdev path
        // never sends; nothing to do.
        if queue != 0 {
            return 0;
        }
        let req = chain.read_request(mem);
        if req.len() < 24 {
            return 0;
        }
        let (ty, flags, fence) = Self::hdr(&req);
        let body = &req[24..];
        let resp = match ty {
            CMD_GET_DISPLAY_INFO => {
                let (w, h) = {
                    let s = self.surface.lock();
                    (s.width(), s.height())
                };
                let mut r = Self::resp(flags, fence, RESP_OK_DISPLAY_INFO);
                // One pmode: rect + enabled + flags (24 bytes).
                r.extend_from_slice(&0u32.to_le_bytes());
                r.extend_from_slice(&0u32.to_le_bytes());
                r.extend_from_slice(&w.to_le_bytes());
                r.extend_from_slice(&h.to_le_bytes());
                r.extend_from_slice(&1u32.to_le_bytes()); // enabled
                r.extend_from_slice(&0u32.to_le_bytes()); // flags
                r
            }
            CMD_RESOURCE_CREATE_2D => {
                let rid = Self::u32_at(body, 0);
                let w = Self::u32_at(body, 4);
                let h = Self::u32_at(body, 8);
                let fmt = Self::u32_at(body, 12);
                let code = if !format_supported(fmt) || w == 0 || h == 0 {
                    RESP_ERR_INVALID_PARAMETER
                } else if self.resources.contains_key(&rid) {
                    RESP_ERR_INVALID_RESOURCE_ID
                } else {
                    self.resources.insert(
                        rid,
                        GpuResource {
                            width: w,
                            height: h,
                            format: fmt,
                            host_buf: alloc::vec![0u8; w as usize * h as usize * 4],
                            backing: Vec::new(),
                        },
                    );
                    RESP_OK_NODATA
                };
                Self::resp(flags, fence, code)
            }
            CMD_RESOURCE_UNREF => {
                let rid = Self::u32_at(body, 0);
                let code = if self.resources.remove(&rid).is_some() {
                    // Unbinding scanouts that pointed at it keeps the
                    // driver's refcounting coherent.
                    for s in self.scanouts.iter_mut() {
                        if *s == Some(rid) {
                            *s = None;
                        }
                    }
                    RESP_OK_NODATA
                } else {
                    RESP_ERR_INVALID_RESOURCE_ID
                };
                Self::resp(flags, fence, code)
            }
            CMD_SET_SCANOUT => {
                // body: rect(16), scanout_id, resource_id.
                let scanout = Self::u32_at(body, 16) as usize;
                let rid = Self::u32_at(body, 20);
                let code = if scanout >= self.scanouts.len() {
                    RESP_ERR_INVALID_SCANOUT_ID
                } else if rid != 0 && !self.resources.contains_key(&rid) {
                    RESP_ERR_INVALID_RESOURCE_ID
                } else {
                    self.scanouts[scanout] = if rid == 0 { None } else { Some(rid) };
                    RESP_OK_NODATA
                };
                Self::resp(flags, fence, code)
            }
            CMD_RESOURCE_FLUSH => {
                let (rx, ry, rw, rh) = Self::rect_at(body, 0);
                let rid = Self::u32_at(body, 16);
                // Unknown resource: error response, complete immediately.
                let known = self.resources.contains_key(&rid);
                if !known {
                    Self::resp(flags, fence, RESP_ERR_INVALID_RESOURCE_ID)
                } else {
                    let ok = self.flush(rid, rx, ry, rw, rh);
                    if ok {
                        Self::resp(flags, fence, RESP_OK_NODATA)
                    } else {
                        // Throttled: the response is already in the chain's
                        // writable buffer but the used-ring entry is
                        // withheld by returning None from process_or_defer;
                        // the re-offer re-runs process (the push is
                        // idempotent via damage dedupe).
                        Self::resp(flags, fence, RESP_OK_NODATA)
                    }
                }
            }
            CMD_TRANSFER_TO_HOST_2D => {
                let (rx, ry, rw, rh) = Self::rect_at(body, 0);
                let offset = u64::from_le_bytes([
                    *body.get(16).unwrap_or(&0),
                    *body.get(17).unwrap_or(&0),
                    *body.get(18).unwrap_or(&0),
                    *body.get(19).unwrap_or(&0),
                    *body.get(20).unwrap_or(&0),
                    *body.get(21).unwrap_or(&0),
                    *body.get(22).unwrap_or(&0),
                    *body.get(23).unwrap_or(&0),
                ]);
                let rid = Self::u32_at(body, 24);
                if self.resources.contains_key(&rid) {
                    self.transfer_to_host(rid, rx, ry, rw, rh, offset, mem);
                    Self::resp(flags, fence, RESP_OK_NODATA)
                } else {
                    Self::resp(flags, fence, RESP_ERR_INVALID_RESOURCE_ID)
                }
            }
            CMD_RESOURCE_ATTACH_BACKING => {
                let rid = Self::u32_at(body, 0);
                let nr = Self::u32_at(body, 4) as usize;
                let mut entries = Vec::new();
                for i in 0..nr {
                    let base = 8 + i * 16;
                    let addr = u64::from_le_bytes([
                        *body.get(base).unwrap_or(&0),
                        *body.get(base + 1).unwrap_or(&0),
                        *body.get(base + 2).unwrap_or(&0),
                        *body.get(base + 3).unwrap_or(&0),
                        *body.get(base + 4).unwrap_or(&0),
                        *body.get(base + 5).unwrap_or(&0),
                        *body.get(base + 6).unwrap_or(&0),
                        *body.get(base + 7).unwrap_or(&0),
                    ]);
                    let len = Self::u32_at(body, base + 8);
                    entries.push((addr, len));
                }
                let code = match self.resources.get_mut(&rid) {
                    Some(r) => {
                        r.backing = entries;
                        RESP_OK_NODATA
                    }
                    None => RESP_ERR_INVALID_RESOURCE_ID,
                };
                Self::resp(flags, fence, code)
            }
            CMD_RESOURCE_DETACH_BACKING => {
                let rid = Self::u32_at(body, 0);
                let code = match self.resources.get_mut(&rid) {
                    Some(r) => {
                        r.backing = Vec::new();
                        RESP_OK_NODATA
                    }
                    None => RESP_ERR_INVALID_RESOURCE_ID,
                };
                Self::resp(flags, fence, code)
            }
            _ => Self::resp(flags, fence, RESP_ERR_UNSPEC),
        };
        chain.write_response(mem, &resp)
    }

    fn process_or_defer(
        &mut self,
        queue: usize,
        chain: &DescriptorChain,
        mem: &mut dyn QueueMem,
    ) -> Option<u32> {
        let written = self.process(queue, chain, mem);
        if self.flush_throttled {
            // Credit bucket empty: withhold completion until the shell
            // drains (refills credits) and the transport re-offers.
            self.flush_throttled = false;
            return None;
        }
        Some(written)
    }
}
