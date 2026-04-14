use core::fmt::Display;

use crate::CoreOpcode;

/// Width of a memory access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemWidth {
    B1,
    B2,
    B4,
    B8,
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

    // ── Memory ────────────────────────────────────────────────────────────
    /// Pop addr from stack; push loaded value (sign-extended if `signed`).
    LoadMem { width: MemWidth, signed: bool },
    /// Pop value then addr from stack; store value at addr.
    StoreMem { width: MemWidth },

    // ── Side effects ──────────────────────────────────────────────────────
    /// Architecture environment call (`ecall` / syscall).
    Ecall,
    /// Unrecoverable error.  JS: `throw new TypeError(msg)`.  WASM: `unreachable`.
    Trap(alloc::string::String),

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
