//! The Vane side of the differential loop: a [`StackHost`] whose state is a
//! plain register image + `vane_arch::Mem`, so the whole guest state can be
//! snapshotted, restored, and compared against Unicorn.
//!
//! The host intentionally holds *all* architectural state in forms that map
//! 1:1 onto Unicorn's model:
//! - RV64: `x0..x31` GPRs plus `rv:f0..f31` FP state slots (raw IEEE bits;
//!   F32 values occupy the low 32 bits, matching Vane's shared StackOp FP
//!   representation).
//! - AArch64: `x0..x30`, `sp`, and the `nzcv:{n,z,c,v}` slots. NZCV flags are
//!   stored as individual 0/1 values (matching `a64_emit`'s lowering) and
//!   repacked into Unicorn's single NZCV register at sync time.
//!
//! Memory lives in a `vane_arch::Mem` (legacy on-demand paging), which the
//! harness mirrors byte-for-byte into Unicorn's address space.

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use vane_arch::{
    Mem, MemWidth, StateSlot,
    interp::{Exit, StackHost, interpret},
};
use vane_target_core::StackOp;

/// Map an AArch64 GPR state-slot name to its register-image index
/// (`a64:x0..a64:x30` → 0..30, `a64:sp` → 31); `None` for anything else.
fn a64_gpr_slot_index(name: &str) -> Option<usize> {
    if name == "a64:sp" {
        return Some(31);
    }
    let rest = name.strip_prefix("a64:x")?;
    let n: usize = rest.parse().ok()?;
    (n <= 30).then_some(n)
}

/// Architecture-agnostic snapshot of everything a step could have touched.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MachineState {
    /// GPR-style registers indexed by number (RV: x0..x31; A64: x0..x30 with
    /// slot 31 = SP). Register 0 is stored too; both ISAs force reads of it
    /// to zero, and the comparator asserts it stayed zero.
    pub regs: [u64; 32],
    /// Named state slots (`rv:f*`, `a64:nzcv:*`, ...). Sorted map so
    /// comparison diffs come out in a stable order.
    pub slots: BTreeMap<String, u64>,
}

impl MachineState {
    /// Read a named state slot, defaulting to 0 when never written.
    pub fn slot(&self, name: &str) -> u64 {
        self.slots.get(name).copied().unwrap_or(0)
    }
}

/// A [`StackHost`] over a `Mem` plus a [`MachineState`] image.
pub struct VaneCrossHost<'a> {
    pub mem: &'a mut Mem,
    pub state: MachineState,
    /// RV64 mode treats register 0 as hardwired zero on reads (matching
    /// `StackOp::LoadReg`'s documented x0 semantics). AArch64 uses state
    /// slots for everything, so this only guards `load_reg`/`store_reg`.
    pub reg_zero_is_hardwired: bool,
}

impl<'a> VaneCrossHost<'a> {
    pub fn new(mem: &'a mut Mem, reg_zero_is_hardwired: bool) -> Self {
        Self {
            mem,
            state: MachineState::default(),
            reg_zero_is_hardwired,
        }
    }

    /// Run one `StackOp` trace to its first `Exit`. The (mutated) state
    /// stays in the host; callers snapshot `host.state` afterwards.
    pub fn run_trace(&mut self, ops: &[StackOp]) -> Exit {
        interpret(ops, self)
    }
}

impl StackHost for VaneCrossHost<'_> {
    fn load_reg(&mut self, r: u8) -> u64 {
        if self.reg_zero_is_hardwired && r == 0 {
            0
        } else {
            self.state.regs[r as usize]
        }
    }

    fn store_reg(&mut self, r: u8, v: u64) {
        if self.reg_zero_is_hardwired && r == 0 {
            return; // x0 writes are architecturally discarded
        }
        self.state.regs[r as usize] = v;
    }

    fn load_state(&mut self, slot: &StateSlot) -> u64 {
        // GPR-shaped AArch64 slots (`a64:xN`, `a64:sp`) mirror into the
        // unified register image so the differential comparator can compare
        // one `[u64; 32]` against Unicorn's register file. Everything else
        // (NZCV, FP slots, scratch) is stored by name.
        if let Some(i) = a64_gpr_slot_index(slot.as_str()) {
            return self.state.regs[i];
        }
        self.state.slots.get(slot.as_str()).copied().unwrap_or(0)
    }

    fn store_state(&mut self, slot: &StateSlot, v: u64) {
        if let Some(i) = a64_gpr_slot_index(slot.as_str()) {
            self.state.regs[i] = v;
            return;
        }
        self.state
            .slots
            .insert(String::from(slot.as_str()), v);
    }

    fn load_mem(&mut self, addr: u64, width: MemWidth, signed: bool) -> u64 {
        let n: u64 = match width {
            MemWidth::B1 => 1,
            MemWidth::B2 => 2,
            MemWidth::B4 => 4,
            MemWidth::B8 => 8,
        };
        let mut value: u64 = 0;
        for i in 0..n {
            value |= (self.mem.read_byte(addr.wrapping_add(i)) as u64) << (8 * i);
        }
        if signed {
            let shift = 64 - 8 * n;
            // Sign-extend from the loaded width.
            ((value << shift) as i64 >> shift) as u64
        } else {
            value
        }
    }

    fn store_mem(&mut self, addr: u64, width: MemWidth, v: u64) {
        let n: u64 = match width {
            MemWidth::B1 => 1,
            MemWidth::B2 => 2,
            MemWidth::B4 => 4,
            MemWidth::B8 => 8,
        };
        for i in 0..n {
            self.mem
                .write_byte(addr.wrapping_add(i), ((v >> (8 * i)) & 0xff) as u8);
        }
    }

    fn byte(&mut self, addr: u64) -> u8 {
        self.mem.read_byte(addr)
    }

    fn ecall(&mut self) -> bool {
        // The differential harness treats every syscall boundary as a skip;
        // returning false stops interpretation with `Exit::Ecall`.
        false
    }
}

/// Build the byte image of a guest memory range for comparison/sync.
pub fn read_mem_range(mem: &Mem, base: u64, len: usize) -> Vec<u8> {
    (0..len as u64)
        .map(|i| mem.read_byte(base + i))
        .collect()
}

/// Write a byte image into a `Mem`.
pub fn write_mem_range(mem: &mut Mem, base: u64, bytes: &[u8]) {
    for (i, b) in bytes.iter().enumerate() {
        mem.write_byte(base + i as u64, *b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec};

    fn host() -> VaneCrossHost<'static> {
        let mem = Box::leak(Box::new(Mem::default()));
        VaneCrossHost::new(mem, true)
    }

    #[test]
    fn load_reg_zero_is_hardwired_in_rv_mode() {
        let mut h = host();
        h.store_reg(0, 0xdead_beef);
        assert_eq!(h.load_reg(0), 0);
        assert_eq!(h.state.regs[0], 0);
        h.store_reg(1, 7);
        assert_eq!(h.load_reg(1), 7);
    }

    #[test]
    fn reg_zero_is_normal_in_a64_mode() {
        let mem = Box::leak(Box::new(Mem::default()));
        let mut h = VaneCrossHost::new(mem, false);
        h.store_reg(0, 0xdead_beef);
        assert_eq!(h.load_reg(0), 0xdead_beef);
    }

    #[test]
    fn memory_roundtrip_and_sign_extension() {
        let mut h = host();
        h.store_mem(0x100, MemWidth::B2, 0xfffe);
        assert_eq!(h.load_mem(0x100, MemWidth::B2, false), 0xfffe);
        assert_eq!(h.load_mem(0x100, MemWidth::B2, true), 0xffff_ffff_ffff_fffe);
        // Unmapped reads are zero, matching `Mem`'s documented semantics.
        assert_eq!(h.load_mem(0x9000, MemWidth::B8, false), 0);
    }

    #[test]
    fn slots_roundtrip() {
        let mut h = host();
        h.store_state(&StateSlot::new("rv:f3"), 1.5f64.to_bits());
        assert_eq!(h.load_state(&StateSlot::new("rv:f3")), 1.5f64.to_bits());
        assert_eq!(h.load_state(&StateSlot::new("rv:f4")), 0);
    }

    #[test]
    fn mem_range_helpers() {
        let mut mem = Mem::default();
        write_mem_range(&mut mem, 0x2000, &[1, 2, 3, 4]);
        assert_eq!(read_mem_range(&mem, 0x2000, 4), vec![1, 2, 3, 4]);
        assert_eq!(read_mem_range(&mem, 0x2000, 6), vec![1, 2, 3, 4, 0, 0]);
    }
}
