//! virtio-gpu 2D-subset device tests: the control-queue flows the Linux
//! `virtio_gpu` driver issues in 2D/fbdev mode, ending in `DisplaySurface`
//! damage — the guest-producer half of the display seam.

use std::collections::BTreeMap;
use vane_riscv::display::DisplaySurface;
use vane_riscv::virtio::{DescriptorChain, QueueMem, VirtioDevice, VirtioMmio};
use vane_riscv::virtio_gpu::VirtioGpuDisplay;

type Surface = vane_arch::Shared<spin::Mutex<DisplaySurface>>;

/// Sparse byte-addressable guest memory for the test.
struct TestMem(BTreeMap<u64, u8>);
impl QueueMem for TestMem {
    fn read(&mut self, addr: u64, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.0.get(&(addr + i as u64)).copied().unwrap_or(0);
        }
    }
    fn write(&mut self, addr: u64, buf: &[u8]) {
        for (i, b) in buf.iter().enumerate() {
            self.0.insert(addr + i as u64, *b);
        }
    }
}
impl TestMem {
    fn put(&mut self, addr: u64, data: &[u8]) {
        self.write(addr, data);
    }
    fn get(&mut self, addr: u64, n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        self.read(addr, &mut b);
        b
    }
}

const PAGE: u64 = 4096;
const DESC: u64 = 4096;
const AVAIL: u64 = 4160;
const REQB: u64 = 0x30000;
const RESPQ: u64 = 0x32000;

/// Submit one controlq command (full legacy queue handshake + kick).
/// Returns `(response bytes, completed)`; `completed == false` means the
/// device DEFERRED the chain (no used-ring entry, no interrupt — the
/// transport stashes it and re-offers on the next `process` pass).
fn submit(d: &mut VirtioMmio, mem: &mut TestMem, req: &[u8]) -> (Vec<u8>, bool) {
    d.write(0x070, 0);
    d.write(0x28, PAGE as u32);
    d.write(0x30, 0);
    d.write(0x38, 4);
    d.write(0x3c, PAGE as u32);
    d.write(0x40, 1);
    mem.put(REQB, req);
    // desc 0: request (readable); desc 1: response (writable).
    mem.put(DESC, &REQB.to_le_bytes());
    mem.put(DESC + 8, &(req.len() as u32).to_le_bytes());
    mem.put(DESC + 12, &1u16.to_le_bytes()); // NEXT
    mem.put(DESC + 14, &1u16.to_le_bytes());
    mem.put(DESC + 16, &RESPQ.to_le_bytes());
    mem.put(DESC + 24, &4096u32.to_le_bytes());
    mem.put(DESC + 28, &2u16.to_le_bytes()); // WRITE
    mem.put(AVAIL + 2, &1u16.to_le_bytes());
    mem.put(AVAIL + 4, &0u16.to_le_bytes());
    d.write(0x050, 0);
    let raised = d.process(mem);
    (mem.get(RESPQ, 4096), raised)
}

/// Re-offer pass (the transport replays deferred chains first).
fn reoffer(d: &mut VirtioMmio, mem: &mut TestMem) -> (Vec<u8>, bool) {
    let raised = d.process(mem);
    (mem.get(RESPQ, 4096), raised)
}

/// Build a controlq request: 24-byte header (type, flags, fence, ctx, pad)
/// + body.
fn cmd(ty: u32, body: &[u8]) -> Vec<u8> {
    let mut r = Vec::new();
    r.extend_from_slice(&ty.to_le_bytes());
    r.extend_from_slice(&0u32.to_le_bytes()); // flags
    r.extend_from_slice(&0xfeedu64.to_le_bytes()); // fence_id
    r.extend_from_slice(&0u32.to_le_bytes()); // ctx_id
    r.extend_from_slice(&0u32.to_le_bytes()); // ring_idx/pad
    r.extend_from_slice(body);
    r
}

fn u32l(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn resp_type(r: &[u8]) -> u32 {
    u32::from_le_bytes([r[0], r[1], r[2], r[3]])
}

/// Rect (x, y, w, h) little-endian body chunk.
fn rect(x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let mut b = u32l(x);
    b.extend(u32l(y));
    b.extend(u32l(w));
    b.extend(u32l(h));
    b
}

fn new_device(w: u32, h: u32, credits: u32) -> (VirtioMmio, TestMem, Surface) {
    let surface = vane_riscv::display::new_shared(w, h, credits);
    let d = VirtioMmio::new(Box::new(VirtioGpuDisplay::new(surface.clone())), 1);
    (d, TestMem(BTreeMap::new()), surface)
}

/// Set up resource 1 (whole-surface size, given format) with backing at
/// 0x40000 and the guest fb already transferred to host.
fn setup_resource(
    d: &mut VirtioMmio,
    mem: &mut TestMem,
    w: u32,
    h: u32,
    format: u32,
    fb: &[u8],
) {
    let mut body = u32l(1);
    body.extend(u32l(w));
    body.extend(u32l(h));
    body.extend(u32l(format));
    let (r, _) = submit(d, mem, &cmd(0x0101, &body));
    assert_eq!(resp_type(&r), 0x1100, "CREATE_2D ok");
    let mut body = u32l(1);
    body.extend(u32l(1)); // nr_entries
    body.extend(&0x40000u64.to_le_bytes());
    body.extend(&u32l(fb.len() as u32));
    body.extend(&0u32.to_le_bytes());
    let (r, _) = submit(d, mem, &cmd(0x0109, &body));
    assert_eq!(resp_type(&r), 0x1100, "ATTACH_BACKING ok");
    mem.put(0x40000, fb);
    let mut body = rect(0, 0, w, h);
    body.extend(&0u64.to_le_bytes()); // offset
    body.extend(u32l(1));
    let (r, _) = submit(d, mem, &cmd(0x0106, &body));
    assert_eq!(resp_type(&r), 0x1100, "TRANSFER_TO_HOST_2D ok");
}

#[test]
fn device_id_and_queues() {
    let surface = vane_riscv::display::new_shared(64, 64, 8);
    let dev = VirtioGpuDisplay::new(surface);
    assert_eq!(dev.device_id(), 16);
    // controlq + cursorq: the Linux driver probes both vqs by name.
    assert_eq!(dev.num_queues(), 2);
}

#[test]
fn get_display_info_reports_the_surface_mode() {
    let (mut d, mut mem, _surface) = new_device(320, 240, 8);
    let (r, _) = submit(&mut d, &mut mem, &cmd(0x0100, &[]));
    assert_eq!(resp_type(&r), 0x1101, "OK_DISPLAY_INFO");
    // pmode rect = surface dims, enabled = 1.
    let w = u32::from_le_bytes([r[32], r[33], r[34], r[35]]);
    let h = u32::from_le_bytes([r[36], r[37], r[38], r[39]]);
    let enabled = u32::from_le_bytes([r[40], r[41], r[42], r[43]]);
    assert_eq!((w, h), (320, 240));
    assert_eq!(enabled, 1);
    // fence echoed (the driver matches completions to fences).
    assert_eq!(u64::from_le_bytes(r[8..16].try_into().unwrap()), 0xfeed);
}

#[test]
fn config_space_num_scanouts() {
    let surface = vane_riscv::display::new_shared(64, 64, 8);
    let mut dev = VirtioGpuDisplay::new(surface);
    let mut cfg = [0u8; 12];
    dev.read_config(0, &mut cfg);
    assert_eq!(
        u32::from_le_bytes(cfg[0..4].try_into().unwrap()),
        0,
        "events_read"
    );
    assert_eq!(
        u32::from_le_bytes(cfg[8..12].try_into().unwrap()),
        1,
        "num_scanouts"
    );
}

#[test]
fn create_attach_transfer_flush_roundtrip() {
    let (mut d, mut mem, surface) = new_device(320, 240, 8);

    // Guest backing: (x, y) -> B8G8R8A8 bytes [x, y, 7, 255].
    let mut fb = Vec::with_capacity(320 * 240 * 4);
    for y in 0..240u32 {
        for x in 0..320u32 {
            fb.extend_from_slice(&[x as u8, y as u8, 7, 255]);
        }
    }
    setup_resource(&mut d, &mut mem, 320, 240, 1, &fb);

    // SET_SCANOUT scanout 0 -> resource 1.
    let mut body = rect(0, 0, 320, 240);
    body.extend(u32l(0)); // scanout_id
    body.extend(u32l(1)); // resource_id
    let (r, _) = submit(&mut d, &mut mem, &cmd(0x0103, &body));
    assert_eq!(resp_type(&r), 0x1100);

    // RESOURCE_FLUSH of a small rect at (10, 20) 4x2.
    let mut body = rect(10, 20, 4, 2);
    body.extend(u32l(1));
    let (r, _) = submit(&mut d, &mut mem, &cmd(0x0104, &body));
    assert_eq!(resp_type(&r), 0x1100, "flush completes with credit");

    // Surface holds converted RGBA; pixel (11, 21) = resource bytes
    // [11, 21, 7, 255] (B8G8R8A8) -> RGBA [7, 21, 11, 255].
    let drained = {
        let mut s = surface.lock();
        assert_eq!(s.pending_len(), 1);
        s.drain()
    };
    assert_eq!(drained.len(), 1);
    let (reg, px) = &drained[0];
    assert_eq!((reg.x, reg.y, reg.w, reg.h), (10, 20, 4, 2));
    // Region row 1, col 1 = surface pixel (11, 21).
    let i = ((21 - 20) * 4 + (11 - 10)) * 4;
    assert_eq!(&px[i..i + 4], &[7, 21, 11, 255]);
    assert_eq!(px.len(), 4 * 2 * 4);
}

#[test]
fn format_conversion_a8r8g8b8() {
    let (mut d, mut mem, surface) = new_device(8, 8, 8);
    // A8R8G8B8: bytes are a, r, g, b.
    let mut fb = vec![0u8; 8 * 8 * 4];
    let i = (4 * 8 + 3) * 4;
    fb[i..i + 4].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    setup_resource(&mut d, &mut mem, 8, 8, 3, &fb);
    let mut body = rect(0, 0, 8, 8);
    body.extend(u32l(1));
    submit(&mut d, &mut mem, &cmd(0x0104, &body));
    let drained = surface.lock().drain();
    let (_, px) = &drained[0];
    assert_eq!(&px[i..i + 4], &[0x22, 0x33, 0x44, 0x11], "A8R8G8B8 -> RGBA");
}

#[test]
fn format_conversion_b8g8r8x8_xrgb() {
    let (mut d, mut mem, surface) = new_device(8, 8, 8);
    // B8G8R8X8 (the XRGB8888 fb format): bytes b, g, r, x -> alpha forced.
    let mut fb = vec![0u8; 8 * 8 * 4];
    let i = 0;
    fb[i..i + 4].copy_from_slice(&[0x44, 0x33, 0x22, 0x00]);
    setup_resource(&mut d, &mut mem, 8, 8, 2, &fb);
    let mut body = rect(0, 0, 8, 8);
    body.extend(u32l(1));
    submit(&mut d, &mut mem, &cmd(0x0104, &body));
    let drained = surface.lock().drain();
    let (_, px) = &drained[0];
    assert_eq!(&px[i..i + 4], &[0x22, 0x33, 0x44, 255], "X forced opaque");
}

#[test]
fn flush_throttles_then_defers_until_drain() {
    // 2-credit surface: two full flushes exhaust the bucket.
    let (mut d, mut mem, surface) = new_device(64, 64, 2);
    let fb = vec![0xAAu8; 64 * 64 * 4];
    setup_resource(&mut d, &mut mem, 64, 64, 2, &fb);
    let flush_all = {
        let mut b = rect(0, 0, 64, 64);
        b.extend(u32l(1));
        cmd(0x0104, &b)
    };

    // Flushes 1 and 2: credits 2 -> 0, both complete.
    let (r, completed) = submit(&mut d, &mut mem, &flush_all);
    assert_eq!(resp_type(&r), 0x1100);
    assert!(completed, "first flush completes");
    let (r, completed) = submit(&mut d, &mut mem, &flush_all);
    assert_eq!(resp_type(&r), 0x1100);
    assert!(completed, "second flush completes");
    // Flush 3: no credits — the chain DEFERS (no used-ring entry, no IRQ;
    // the guest's flush ioctl blocks until the shell drains).
    let (_, completed) = submit(&mut d, &mut mem, &flush_all);
    assert!(!completed, "third flush defers on empty credits");
    assert!(!d.interrupt_pending(), "deferral must not raise the IRQ");
    // All three flushes pushed the IDENTICAL full-frame rect, so damage
    // dedupe collapsed them into one region (the throttle's job is damage
    // VOLUME, and identical rects are free).
    {
        let s = surface.lock();
        assert_eq!(s.pending_len(), 1, "identical rects dedupe to one region");
        assert_eq!(s.credits(), 0, "bucket drained to zero");
    }
    // Drain (the shell's rAF): credits refill, damage delivered.
    let drained = surface.lock().drain();
    assert_eq!(drained.len(), 1);
    assert_eq!(surface.lock().credits(), 2, "drain refills the bucket");
    // Re-offer (the transport replays the stashed chain): completes now.
    let (r, completed) = reoffer(&mut d, &mut mem);
    assert_eq!(resp_type(&r), 0x1100);
    assert!(completed, "flush completes after drain refills credits");
    // The re-offer's damage dedupes (identical rect already covered).
    assert_eq!(surface.lock().pending_len(), 1);
}

#[test]
fn unknown_resource_and_bad_scanout_error() {
    let (mut d, mut mem, _surface) = new_device(64, 64, 8);
    // Flush of a resource that doesn't exist.
    let mut body = rect(0, 0, 4, 4);
    body.extend(u32l(99));
    let (r, _) = submit(&mut d, &mut mem, &cmd(0x0104, &body));
    assert_eq!(resp_type(&r), 0x1203, "ERR_INVALID_RESOURCE_ID");
    // Scanout out of range.
    let mut body = rect(0, 0, 4, 4);
    body.extend(u32l(9)); // scanout_id
    body.extend(u32l(0));
    let (r, _) = submit(&mut d, &mut mem, &cmd(0x0103, &body));
    assert_eq!(resp_type(&r), 0x1202, "ERR_INVALID_SCANOUT_ID");
    // Unknown command.
    let (r, _) = submit(&mut d, &mut mem, &cmd(0x0777, &[]));
    assert_eq!(resp_type(&r), 0x1200, "ERR_UNSPEC");
    // Unsupported pixel format rejected at create.
    let mut body = u32l(3);
    body.extend(u32l(8));
    body.extend(u32l(8));
    body.extend(u32l(55)); // not in the supported set
    let (r, _) = submit(&mut d, &mut mem, &cmd(0x0101, &body));
    assert_eq!(resp_type(&r), 0x1205, "ERR_INVALID_PARAMETER");
}

#[test]
fn cursor_queue_completes_empty() {
    let surface = vane_riscv::display::new_shared(64, 64, 8);
    let mut dev = VirtioGpuDisplay::new(surface);
    let chain = DescriptorChain {
        head: 0,
        readable: vec![],
        writable: vec![],
    };
    let mut mem = TestMem(BTreeMap::new());
    assert_eq!(dev.process(1, &chain, &mut mem), 0, "cursorq is a no-op");
}

#[test]
fn unref_cleans_scanout_binding() {
    let (mut d, mut mem, _surface) = new_device(64, 64, 8);
    // Create + scanout bind + unref: the scanout clears with it.
    let mut body = u32l(5);
    body.extend(u32l(64));
    body.extend(u32l(64));
    body.extend(u32l(2));
    submit(&mut d, &mut mem, &cmd(0x0101, &body));
    let mut body = rect(0, 0, 64, 64);
    body.extend(u32l(0));
    body.extend(u32l(5));
    submit(&mut d, &mut mem, &cmd(0x0103, &body));
    submit(&mut d, &mut mem, &cmd(0x0102, &u32l(5)));
    // Flush after unref: resource gone -> error, not a crash.
    let mut body = rect(0, 0, 4, 4);
    body.extend(u32l(5));
    let (r, _) = submit(&mut d, &mut mem, &cmd(0x0104, &body));
    assert_eq!(resp_type(&r), 0x1203);
}
