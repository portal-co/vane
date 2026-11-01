//! Guest display seam: a shared framebuffer with damage tracking, input
//! queueing, and frame credits — the guest-visible surface both shells
//! (native AppKit, web canvas) render from, independent of how the guest
//! produces pixels.
//!
//! Push model, subimage API (shaped like `texSubImage2D`): the guest side
//! pushes `{x, y, width, height, pixels}` updates; the host drains
//! accumulated damage regions (with pixels re-sliced from the framebuffer,
//! so overlapping pushes naturally resolve to the latest state). Every push
//! consumes a frame credit; credits refill on drain, so a runaway guest
//! stalls instead of flooding the host.
//!
//! Pixel format v1 is RGBA8888 (canvas `ImageData` byte order; the native
//! shell converts when blitting). WebGPU cutover (if it happens at all)
//! revisits the transport, not this seam.
//!
//! v1 scope: display + input only. There is deliberately no virtio device
//! here yet — the surface is standalone ([`Shared`] owned by the shell) so
//! a guest-less test pattern exercises the full host pipeline first; the
//! virtio-display device will produce into the same surface.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt::Formatter;

/// One damage rectangle in framebuffer pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl core::fmt::Display for Region {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "({},{} {}x{})", self.x, self.y, self.w, self.h)
    }
}

/// Guest input event queued for the guest to consume. v1 carries raw codes;
/// the guest-side device (and its DTB wiring) decides the mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// Keyboard event; `code` is the producer's key code (web: KeyboardEvent
    /// code hashed to u32 by the shell), `pressed` = down.
    Key { code: u32, pressed: bool },
    /// Pointer event. `kind`: 0 = move, 1 = button down, 2 = button up.
    Pointer {
        x: u32,
        y: u32,
        buttons: u8,
        kind: u8,
    },
}

/// Outcome of a subimage push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushStatus {
    /// Applied and recorded as damage.
    Applied,
    /// Applied (the in-bounds part) but no credit was left; the caller must
    /// retry after the host drains. Pixels still landed in the framebuffer
    /// (a DMA'd framebuffer write cannot be un-written) — the throttle is
    /// about damage-record volume, not visibility.
    Throttled,
    /// Entirely outside the framebuffer; nothing changed.
    Outside,
}

/// The guest display surface: framebuffer + pending damage + input queue +
/// frame credits. Shared between the shell (drain/input/test pattern) and,
/// later, the guest display device (push).
pub struct DisplaySurface {
    width: u32,
    height: u32,
    /// RGBA8888, `width * height * 4` bytes.
    fb: Vec<u8>,
    /// Damage regions awaiting drain, oldest first. Fully-contained regions
    /// are skipped (the container already covers them).
    pending: Vec<Region>,
    /// Frame credits: each push consumes one, refill happens on drain.
    credits: u32,
    max_credits: u32,
    /// Input events queued by the shell, FIFO for the guest device.
    input: VecDeque<InputEvent>,
    /// Cap on queued input (a flood of pointer moves must not grow this
    /// unboundedly); oldest events are dropped past the cap.
    input_cap: usize,
}

impl DisplaySurface {
    /// A `width * height` RGBA surface. `max_credits` bounds the pushes
    /// between drains (the frame-credit bucket).
    pub fn new(width: u32, height: u32, max_credits: u32) -> Self {
        Self {
            width,
            height,
            fb: alloc::vec![0u8; (width as usize) * (height as usize) * 4],
            pending: Vec::new(),
            credits: max_credits,
            max_credits,
            input: VecDeque::new(),
            input_cap: 1024,
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Direct read access to the framebuffer (RGBA8888).
    pub fn fb(&self) -> &[u8] {
        &self.fb
    }

    /// Push a subimage update. Rows are tightly packed (`w * 4` bytes per
    /// row). Out-of-bounds parts are clipped (texSubImage2D semantics); a
    /// fully-outside push is [`PushStatus::Outside`].
    pub fn push(&mut self, r: Region, pixels: &[u8]) -> PushStatus {
        // Clip to the framebuffer.
        let x0 = r.x.min(self.width);
        let y0 = r.y.min(self.height);
        let x1 = r.x.saturating_add(r.w).min(self.width);
        let y1 = r.y.saturating_add(r.h).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return PushStatus::Outside;
        }
        let cw = (x1 - x0) as usize;
        let ch = (y1 - y0) as usize;
        // Copy the clipped sub-rectangle from the caller's (unclipped) row
        // layout into the framebuffer.
        let row = cw * 4;
        for row_i in 0..ch {
            let src_y = (y0 as usize) - (r.y as usize) + row_i;
            let src = ((r.w as usize) * 4 * src_y) + ((x0 as usize) - (r.x as usize)) * 4;
            if src + row > pixels.len() {
                // Short pixels buffer: stop at what was provided rather
                // than panicking on malformed guest data.
                break;
            }
            let dst = ((y0 as usize) + row_i) * (self.width as usize) * 4 + (x0 as usize) * 4;
            self.fb[dst..dst + row].copy_from_slice(&pixels[src..src + row]);
        }
        let clipped = Region {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        };
        // Containment dedupe: skip if an existing pending region already
        // covers it. Partial overlaps are left as separate entries — drain
        // re-slices live pixels so ordering cannot produce stale output.
        if self.pending.iter().any(|p| covers(*p, clipped)) {
            // Already covered: still costs the credit.
            return self.spend_credit(PushStatus::Applied);
        }
        self.pending.push(clipped);
        self.spend_credit(PushStatus::Applied)
    }

    fn spend_credit(&mut self, ok: PushStatus) -> PushStatus {
        if self.credits == 0 {
            PushStatus::Throttled
        } else {
            self.credits -= 1;
            ok
        }
    }

    /// Drain accumulated damage: returns the pending regions with their
    /// current pixels sliced out of the framebuffer, and refills the frame
    /// credits. Empty result = nothing to render.
    pub fn drain(&mut self) -> Vec<(Region, Vec<u8>)> {
        let stride = (self.width as usize) * 4;
        let mut out = Vec::with_capacity(self.pending.len());
        for r in core::mem::take(&mut self.pending) {
            let w = r.w as usize;
            let h = r.h as usize;
            let mut px = alloc::vec![0u8; w * h * 4];
            for row in 0..h {
                let src = ((r.y as usize) + row) * stride + (r.x as usize) * 4;
                let dst = row * w * 4;
                px[dst..dst + w * 4].copy_from_slice(&self.fb[src..src + w * 4]);
            }
            out.push((r, px));
        }
        self.credits = self.max_credits;
        out
    }

    /// Pending damage count (for tests and shell diagnostics).
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn credits(&self) -> u32 {
        self.credits
    }

    /// Fill the whole framebuffer with a deterministic test pattern and
    /// record one full-frame damage region — the guest-less path that lets
    /// a shell fixture exercise the full display pipeline. Pattern: 32px
    /// checkerboard of two hue-shifted gradients plus a 8px white frame
    /// marker every 256px tile boundary, so blit misalignment, stride bugs
    /// and origin flips are all visible at a glance.
    pub fn fill_test_pattern(&mut self) {
        let w = self.width as usize;
        let h = self.height as usize;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let checker = ((x / 32) + (y / 32)) % 2 == 0;
                let (r, g, b) = if checker {
                    (x as u8, y as u8, 128)
                } else {
                    (128, (255 - x) as u8, (255 - y) as u8)
                };
                let edge = (x % 256 < 8) || (y % 256 < 8);
                let (r, g, b) = if edge { (255, 255, 255) } else { (r, g, b) };
                self.fb[i] = r;
                self.fb[i + 1] = g;
                self.fb[i + 2] = b;
                self.fb[i + 3] = 255;
            }
        }
        self.pending.push(Region {
            x: 0,
            y: 0,
            w: self.width,
            h: self.height,
        });
    }

    /// Queue an input event for the guest. Past the cap, the oldest event
    /// is dropped (pointer-move floods degrade to coarser motion; keys are
    /// rare enough in practice that the cap only bites under floods).
    pub fn push_input(&mut self, ev: InputEvent) {
        if self.input.len() >= self.input_cap {
            self.input.pop_front();
        }
        self.input.push_back(ev);
    }

    /// Guest-side consume: pop the next input event, FIFO.
    pub fn take_input(&mut self) -> Option<InputEvent> {
        self.input.pop_front()
    }

    pub fn input_len(&self) -> usize {
        self.input.len()
    }
}

/// Does `a` fully cover `b`?
fn covers(a: Region, b: Region) -> bool {
    a.x <= b.x && a.y <= b.y && a.x + a.w >= b.x + b.w && a.y + a.h >= b.y + b.h
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn push_blits_and_clips() {
        let mut s = DisplaySurface::new(4, 4, 8);
        // 2x2 red block at the source origin, garbage elsewhere, pushed at
        // (2,2) sized 4x4 on a 4x4 fb: the bottom/right garbage must clip
        // away at the framebuffer edge and the damage must record the
        // CLIPPED rectangle.
        let mut px = vec![9u8; 4 * 4 * 4];
        for y in 0..2 {
            for x in 0..2 {
                let i = (y * 4 + x) * 4;
                px[i] = 255;
                px[i + 1] = 0;
                px[i + 2] = 0;
                px[i + 3] = 255;
            }
        }
        assert_eq!(
            s.push(
                Region {
                    x: 2,
                    y: 2,
                    w: 4,
                    h: 4
                },
                &px
            ),
            PushStatus::Applied
        );
        let fb = s.fb();
        // (0,0) untouched, (2,2) and (3,3) red, nothing above/left of x=2.
        assert_eq!(&fb[0..4], &[0, 0, 0, 0]);
        assert_eq!(&fb[(2 * 4 + 2) * 4..(2 * 4 + 2) * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(&fb[(3 * 4 + 3) * 4..(3 * 4 + 3) * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(&fb[(1 * 4 + 2) * 4..(1 * 4 + 2) * 4 + 4], &[0, 0, 0, 0]);
        // The damage record is the clipped rect, not the requested one.
        assert_eq!(s.pending_len(), 1);
        let drained = s.drain();
        assert_eq!(
            drained[0].0,
            Region {
                x: 2,
                y: 2,
                w: 2,
                h: 2
            }
        );
        assert_eq!(&drained[0].1[0..4], &[255, 0, 0, 255]);
        assert_eq!(drained[0].1.len(), 2 * 2 * 4);
    }

    #[test]
    fn fully_outside_push_is_outside() {
        let mut s = DisplaySurface::new(4, 4, 8);
        assert_eq!(
            s.push(
                Region {
                    x: 10,
                    y: 10,
                    w: 2,
                    h: 2
                },
                &[]
            ),
            PushStatus::Outside
        );
        assert_eq!(s.pending_len(), 0);
        // Zero-size too.
        assert_eq!(
            s.push(
                Region {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0
                },
                &[]
            ),
            PushStatus::Outside
        );
    }

    #[test]
    fn short_pixel_buffer_does_not_panic() {
        let mut s = DisplaySurface::new(4, 4, 8);
        // Truncated rows: the blit stops at what was provided.
        let px = vec![7u8; 5];
        assert_eq!(
            s.push(
                Region {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2
                },
                &px
            ),
            PushStatus::Applied
        );
    }

    #[test]
    fn credits_throttle_and_refill_on_drain() {
        let mut s = DisplaySurface::new(4, 4, 2);
        let px = vec![1u8; 4 * 4];
        assert_eq!(
            s.push(
                Region {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1
                },
                &px
            ),
            PushStatus::Applied
        );
        assert_eq!(
            s.push(
                Region {
                    x: 2,
                    y: 2,
                    w: 1,
                    h: 1
                },
                &px
            ),
            PushStatus::Applied
        );
        assert_eq!(s.credits(), 0);
        assert_eq!(
            s.push(
                Region {
                    x: 0,
                    y: 2,
                    w: 1,
                    h: 1
                },
                &px
            ),
            PushStatus::Throttled
        );
        // The throttled push still blitted and recorded.
        assert_eq!(s.pending_len(), 3);
        assert_eq!(s.drain().len(), 3);
        assert_eq!(s.credits(), 2);
        assert_eq!(s.pending_len(), 0);
        assert_eq!(s.drain().len(), 0);
    }

    #[test]
    fn contained_push_dedupes_but_still_costs_credit() {
        let mut s = DisplaySurface::new(8, 8, 4);
        let px = vec![1u8; 8 * 8 * 4];
        s.push(
            Region {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            &px,
        );
        let n = s.pending_len();
        s.push(
            Region {
                x: 1,
                y: 1,
                w: 2,
                h: 2,
            },
            &px,
        );
        assert_eq!(s.pending_len(), n, "contained region must not duplicate");
        assert_eq!(s.credits(), 2, "dedupe still consumes the credit");
    }

    #[test]
    fn drain_returns_live_pixels_per_region() {
        let mut s = DisplaySurface::new(2, 2, 8);
        // Fill all blue, then overwrite (1,1) green: two overlapping pushes.
        let blue = [0u8, 0, 255, 255];
        let mut all = Vec::new();
        for _ in 0..4 {
            all.extend_from_slice(&blue);
        }
        s.push(
            Region {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            &all,
        );
        let green = [0u8, 255, 0, 255];
        s.push(
            Region {
                x: 1,
                y: 1,
                w: 1,
                h: 1,
            },
            &green,
        );
        let drained = s.drain();
        // The green push was contained by the full-frame damage and got
        // deduped — but the drained region's pixels must still show the
        // CURRENT state (green at (1,1)): drain re-slices the framebuffer
        // at drain time rather than snapshotting at push time.
        assert_eq!(drained.len(), 1);
        let (_, px) = &drained[0];
        assert_eq!(&px[0..4], &blue, "(0,0) still blue");
        let i = (1 * 2 + 1) * 4;
        assert_eq!(&px[i..i + 4], &green, "(1,1) shows the later green write");
    }

    #[test]
    fn test_pattern_is_deterministic_and_marked_full_frame() {
        let mut a = DisplaySurface::new(64, 64, 8);
        let mut b = DisplaySurface::new(64, 64, 8);
        a.fill_test_pattern();
        b.fill_test_pattern();
        assert_eq!(a.fb(), b.fb(), "pattern must be deterministic");
        assert_eq!(a.pending_len(), 1);
        let d = a.drain();
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].0,
            Region {
                x: 0,
                y: 0,
                w: 64,
                h: 64
            }
        );
        assert_eq!(d[0].1.len(), 64 * 64 * 4);
        // Opaque alpha everywhere.
        assert!(d[0].1.iter().skip(3).step_by(4).all(|&v| v == 255));
        // Edge marker: (0,0) is white (inside the 8px frame).
        assert_eq!(&d[0].1[0..4], &[255, 255, 255, 255]);
        // Checker center: (48,48) is in the edge-free zone of tile 1,1 →
        // checker (48/32 + 48/32) % 2 == 0 → (x, y, 128) = (48, 48, 128).
        let i = (48 * 64 + 48) * 4;
        assert_eq!(&d[0].1[i..i + 4], &[48, 48, 128, 255]);
    }

    #[test]
    fn input_queue_fifo_with_cap() {
        let mut s = DisplaySurface::new(4, 4, 8);
        for i in 0..1100u32 {
            s.push_input(InputEvent::Key {
                code: i,
                pressed: true,
            });
        }
        assert_eq!(s.input_len(), 1024, "cap enforced");
        // Oldest dropped: first event is code 76 (1100 - 1024).
        assert_eq!(
            s.take_input(),
            Some(InputEvent::Key {
                code: 76,
                pressed: true
            })
        );
        s.push_input(InputEvent::Pointer {
            x: 3,
            y: 4,
            buttons: 1,
            kind: 1,
        });
        // Drain all remaining keys then get the pointer event last (FIFO).
        let mut last = None;
        while let Some(ev) = s.take_input() {
            last = Some(ev);
        }
        assert_eq!(
            last,
            Some(InputEvent::Pointer {
                x: 3,
                y: 4,
                buttons: 1,
                kind: 1
            })
        );
        assert_eq!(s.take_input(), None);
    }
}
