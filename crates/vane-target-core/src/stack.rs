use core::fmt::Display;

use crate::CoreOpcode;
use os_target_core::{Backend as OsBackend, MemWidth as OsMemWidth, OsOp};

/// Width of a memory access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemWidth {
    B1,
    B2,
    B4,
    B8,
}

/// IEEE-754 scalar width used by architecture-neutral floating-point stack
/// operations. Floating values travel through the stack as raw IEEE bits in a
/// `u64`; `F32` occupies the low 32 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatWidth {
    F32,
    F64,
}

/// Scalar floating-point binary operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Min,
    Max,
}

/// Scalar floating-point unary operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatUnaryOp {
    Abs,
    Neg,
    Sqrt,
}

/// Direction of an integer/scalar-FP conversion. Integer values are carried as
/// u64; `int_width` selects W (32) versus X (64) representation.

/// A named architectural state slot.
///
/// Unlike [`StackOp::LoadReg`], a state slot has no implicit RISC-V x0
/// behaviour. Architectures use it for ordinary registers, flags, and special
/// registers whose identity cannot be represented by the historic `x{n}`
/// register convention.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StateSlot(pub alloc::string::String);

impl StateSlot {
    pub fn new(name: impl Into<alloc::string::String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A single operation in the shared stack-based IR.
///
/// All variants hold only owned data, so `StackOp` is `'static`.  Structural
/// control-flow constructs (`Loop`/`If`) are expressed as matched begin/end
/// pairs in the flat opcode stream rather than as nested iterators, so
/// consumers maintain a depth counter or an explicit stack instead of
/// recursing into sub-iterators.
pub enum StackOp {
    // ── Control flow ──────────────────────────────────────────────────────
    /// Open a named infinite loop.  All opcodes up to the matching `LoopEnd`
    /// form the loop body.  Created by `CoreOpcode::loop`.
    LoopBegin {
        label: alloc::string::String,
    },
    /// Close the most recently opened `LoopBegin`.
    LoopEnd,

    /// Unconditional backward branch.
    /// `relative_depth` counts open loops outward from the branch site
    /// (WASM semantics).  `label` is used by the JS shim (`continue label`).
    Br {
        relative_depth: u32,
        label: alloc::string::String,
    },

    /// Conditional split: pop top-of-stack (u64; 0 = false), enter the
    /// then-branch; otherwise enter the else-branch.
    ///
    /// Stream layout: `IfBegin … IfElse … IfEnd` where the first `…` is the
    /// then-body and the second `…` is the else-body.  Both spans may be
    /// empty.  `IfElse` is always emitted even when the else-body is empty.
    IfBegin,
    /// Separates the then-body from the else-body inside an `IfBegin…IfEnd`.
    IfElse,
    /// Closes the most recently opened `IfBegin`.
    IfEnd,

    // ── Stack literals ────────────────────────────────────────────────────
    /// Push a 64-bit unsigned constant.
    PushImm(u64),

    // ── Register file ─────────────────────────────────────────────────────
    /// Push value of architectural register `r`.  Register 0 always pushes 0.
    LoadReg(u8),
    /// Pop and write to architectural register `r`.  Register 0 discards.
    StoreReg(u8),

    /// Pop and discard a value. This is architecture-neutral; do not use a
    /// distinguished register as a discard sink.
    Drop,

    /// Push a named architecture-defined state slot. Unlike `LoadReg`, this
    /// never reserves slot zero and is suitable for AArch64 X0, SP, NZCV, and
    /// scalar V-register raw-bit state.
    LoadState(StateSlot),
    /// Pop and write a named architecture-defined state slot.
    StoreState(StateSlot),

    // ── 64-bit integer arithmetic ─────────────────────────────────────────
    Add64,
    Sub64,
    Mul64,
    DivU64,
    DivS64,
    RemU64,
    RemS64,
    And64,
    Or64,
    Xor64,
    Shl64,
    ShrU64,
    ShrS64,
    /// High 64 bits of unsigned 64×64 multiply.
    MulHU64,
    /// High 64 bits of signed×unsigned 64×64 multiply.
    MulHSU64,
    /// High 64 bits of signed 64×64 multiply.
    MulHS64,

    // ── 32-bit arithmetic (result sign-extended to i64, stored as u64) ────
    Add32,
    Sub32,
    Mul32,
    DivU32,
    DivS32,
    RemU32,
    RemS32,
    Shl32,
    ShrU32,
    ShrS32,

    // ── Conversions ───────────────────────────────────────────────────────
    /// Keep low 32 bits; zero-extend to 64.
    Trunc32,
    /// Sign-extend bits [31:0] to 64.
    SignExt32,
    /// Reinterpret u64 bit-pattern as i64 (`BigInt.asIntN(64,…)` in JS; no-op in WASM).
    AsSigned64,
    /// Reinterpret i64 bit-pattern as u64 (`BigInt.asUintN(64,…)` in JS; no-op in WASM).
    AsUnsigned64,

    // ── Comparisons (push 1u64 or 0u64) ──────────────────────────────────
    Eq64,
    Ne64,
    LtU64,
    LtS64,
    GeU64,
    GeS64,

    // ── Scalar IEEE-754 operations ───────────────────────────────────────
    /// Pop raw IEEE bits and push the result's raw bits. `F32` rounds each
    /// operation at single precision; state slots retain the result bits.
    FloatUnary {
        width: FloatWidth,
        op: FloatUnaryOp,
    },
    /// Pop rhs then lhs raw IEEE bits and push raw result bits.
    FloatBinary {
        width: FloatWidth,
        op: FloatBinOp,
    },
    /// Pop rhs then lhs raw IEEE bits and push `1` when lhs < rhs. NaNs yield
    /// zero, matching ordered AArch64/RISC-V predicate use.
    FloatLt {
        width: FloatWidth,
    },
    /// Pop rhs then lhs raw IEEE bits and push `1` when lhs == rhs. NaNs yield
    /// zero.
    FloatEq {
        width: FloatWidth,
    },
    /// Pop rhs then lhs raw IEEE bits and push `1` when either operand is
    /// NaN, enabling guest ordered/unordered FP comparison semantics.
    FloatUnordered {
        width: FloatWidth,
    },
    /// Pop an integer and push scalar IEEE bits.
    IntToFloat {
        width: FloatWidth,
        signed: bool,
        int_width: u8,
    },
    /// Pop scalar IEEE bits and push an integer rounded toward zero.
    FloatToInt {
        width: FloatWidth,
        signed: bool,
        int_width: u8,
    },
    /// Pop raw IEEE bits at `from` width and push bits rounded/converted to
    /// `to` width.
    FloatConvert {
        from: FloatWidth,
        to: FloatWidth,
    },

    // ── Memory ────────────────────────────────────────────────────────────
    /// Pop addr from stack; push loaded value (sign-extended if `signed`).
    LoadMem {
        width: MemWidth,
        signed: bool,
    },
    /// Pop value then addr from stack; store value at addr.
    StoreMem {
        width: MemWidth,
    },

    // ── Side effects ──────────────────────────────────────────────────────
    /// Architecture environment call (`ecall` / syscall).
    Ecall,
    /// Unrecoverable error.  JS: `throw new TypeError(msg)`.  WASM: `unreachable`.
    Trap(alloc::string::String),
    /// Pop a 64-bit address from the stack; exit the current JIT block and
    /// tail-call the JIT dispatcher for that address.
    ///
    /// JS: `return ()=>J(addr);`  WASM: `unreachable` (not yet implemented)
    TailCall,
    /// Emit a diagnostic log message.  No effect on machine state.
    ///
    /// JS: `console.log(msg)`  WASM: no-op
    Log(alloc::string::String),
    /// Execute a raw privileged (or otherwise non-lowered) instruction inline
    /// via the host's architecture handler (`StackHost::privileged`), keeping
    /// the semantics in one place (the hart) instead of boundarying the trace
    /// out to a separate step engine. `pc` is the instruction's guest address
    /// (for trap reporting and the continue/divert decision). The trace
    /// continues on `PrivOutcome::Continue` and ends (`TailCall`) on
    /// `PrivOutcome::Divert`.
    Privileged {
        pc: u64,
        raw: u32,
    },

    // ── Instruction integrity check ───────────────────────────────────────
    /// Verify that the bytes at `addr` in guest memory match `expected`.
    /// If not, invalidate the JIT cache entry keyed by `miss_label` and exit
    /// the current compiled block.
    ///
    /// `expected` is a raw byte slice so that any ISA can use it:
    /// 4 bytes for fixed-width RISC-V, 1–15 bytes for variable-length x86, etc.
    CheckCode {
        addr: u64,
        expected: alloc::boxed::Box<[u8]>,
        miss_label: alloc::string::String,
    },
}

impl<'op> CoreOpcode<'op> for StackOp {
    fn r#loop(
        body: impl Iterator<Item = Self> + 'op,
        name: impl Display,
    ) -> impl Iterator<Item = Self> + 'op {
        let label = alloc::format!("{name}");
        core::iter::once(StackOp::LoopBegin { label })
            .chain(body)
            .chain(core::iter::once(StackOp::LoopEnd))
    }

    fn br(relative_depth: u32, name: impl Display) -> impl Iterator<Item = Self> {
        core::iter::once(StackOp::Br {
            relative_depth,
            label: alloc::format!("{name}"),
        })
    }
}

fn os_mem_width_to_stack(width: OsMemWidth) -> Option<MemWidth> {
    match width {
        OsMemWidth::W8 => Some(MemWidth::B1),
        OsMemWidth::W16 => Some(MemWidth::B2),
        OsMemWidth::W32 => Some(MemWidth::B4),
        OsMemWidth::W64 => Some(MemWidth::B8),
        OsMemWidth::W128 => None,
    }
}

/// A [`Backend`] for `os-target-core` that lowers [`OsOp`] into vane's
/// [`StackOp`] IR.
///
/// This is intentionally one-way: shared OS glue emits `OsOp`, and vane's
/// existing interpreter / JS renderer consume the resulting `StackOp`s.
pub struct StackOpBackend {
    ops: alloc::vec::Vec<StackOp>,
}

impl StackOpBackend {
    pub fn new() -> Self {
        Self {
            ops: alloc::vec::Vec::new(),
        }
    }

    pub fn into_ops(self) -> alloc::vec::Vec<StackOp> {
        self.ops
    }

    fn push(&mut self, op: StackOp) {
        self.ops.push(op);
    }
}

impl Default for StackOpBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl OsBackend for StackOpBackend {
    fn op(&mut self, op: OsOp) {
        match op {
            OsOp::PushU64(v) => self.push(StackOp::PushImm(v)),
            OsOp::PushU32(v) => self.push(StackOp::PushImm(v as u64)),
            OsOp::Pop => self.push(StackOp::Drop),
            OsOp::Load { width, signed } => {
                if let Some(w) = os_mem_width_to_stack(width) {
                    self.push(StackOp::LoadMem { width: w, signed });
                } else {
                    self.push(StackOp::Trap(alloc::string::String::from(
                        "unsupported load width",
                    )));
                }
            }
            OsOp::Store { width } => {
                if let Some(w) = os_mem_width_to_stack(width) {
                    self.push(StackOp::StoreMem { width: w });
                } else {
                    self.push(StackOp::Trap(alloc::string::String::from(
                        "unsupported store width",
                    )));
                }
            }
            OsOp::Ecall { .. } => self.push(StackOp::Ecall),
            OsOp::Jump { target } => {
                self.push(StackOp::PushImm(target));
                self.push(StackOp::TailCall);
            }
            OsOp::Trap => self.push(StackOp::Trap(alloc::string::String::from(
                "os emulation trap",
            ))),
            OsOp::TailCall { helper } => {
                // Static helper tail-calls do not exist in `StackOp`; record
                // the symbol and then perform a dynamic tail-call dispatch.
                self.push(StackOp::Log(alloc::format!("tailcall: {helper}")));
                self.push(StackOp::TailCall);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_pop_becomes_store_discard() {
        let mut b = StackOpBackend::new();
        b.op(OsOp::PushU64(42));
        b.op(OsOp::Pop);
        let ops = b.into_ops();
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], StackOp::PushImm(42)));
        assert!(matches!(ops[1], StackOp::Drop));
    }

    #[test]
    fn load_maps_to_load_mem() {
        let mut b = StackOpBackend::new();
        b.op(OsOp::PushU64(0x100));
        b.op(OsOp::Load {
            width: OsMemWidth::W32,
            signed: true,
        });
        let ops = b.into_ops();
        assert!(matches!(
            ops[1],
            StackOp::LoadMem {
                width: MemWidth::B4,
                signed: true
            }
        ));
    }

    #[test]
    fn store_swaps_naively() {
        let mut b = StackOpBackend::new();
        b.op(OsOp::PushU64(0x200)); // value
        b.op(OsOp::PushU64(0x100)); // addr
        b.op(OsOp::Store {
            width: OsMemWidth::W16,
        });
        let ops = b.into_ops();
        assert!(matches!(
            ops[2],
            StackOp::StoreMem {
                width: MemWidth::B2
            }
        ));
    }

    #[test]
    fn jump_pushes_target_then_tailcalls() {
        let mut b = StackOpBackend::new();
        b.op(OsOp::Jump { target: 0x4000 });
        let ops = b.into_ops();
        assert!(matches!(ops[0], StackOp::PushImm(0x4000)));
        assert!(matches!(ops[1], StackOp::TailCall));
    }

    #[test]
    fn w128_load_traps() {
        let mut b = StackOpBackend::new();
        b.op(OsOp::PushU64(0));
        b.op(OsOp::Load {
            width: OsMemWidth::W128,
            signed: false,
        });
        assert!(matches!(b.into_ops().last(), Some(StackOp::Trap(_))));
    }
}
