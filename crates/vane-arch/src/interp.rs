//! A native `StackOp` interpreter — "Mode C" from the plan.
//!
//! Nothing in this codebase interprets `StackOp` directly today:
//! `render_stack_to_js`/`render_stack_to_wasm` (`crate::template`) *compile*
//! it, and the only tree-walking interpreter (`Reactor::interp` in
//! `crates/vane/src/lib.rs`) works on raw `rv_asm::Inst`, not `StackOp`. This
//! module fills that gap so a server can execute a trace directly against
//! local state, selectively delegating only the operations that actually
//! need a real host (`StackHost::ecall`) — see [`AsyncStackHost`] for the
//! async twin used when that delegation may need to `.await`.
//!
//! `ops` must be a fully materialized slice (not a lazy iterator): `Br`
//! resolves by jumping the instruction pointer backward to a previously
//! recorded loop-start index, which requires random access. `rv_emit`
//! (`vane-riscv/src/template/riscv.rs`) already builds a `Vec<StackOp>`
//! eagerly, so callers have one on hand regardless.
//!
//! Arithmetic here is written to match `render_stack_to_js`'s formulas
//! (`crate::template`) wherever they're unambiguous (wrapping 64-bit ops,
//! division/remainder-by-zero returning all-ones, signed division/remainder
//! via `wrapping_div`/`wrapping_rem` to match BigInt's `i64::MIN / -1`
//! wraparound). One deliberate divergence: shift amounts are masked to the
//! low 6 bits (`b & 63`) as real RV64 hardware does, rather than replicating
//! the JS renderer's "shift by the full unmasked BigInt, then truncate to 64
//! bits" behavior for out-of-range shift amounts — `StackOp::Shl64`/`ShrU64`/
//! `ShrS64` never mask their shift-amount operand themselves (`rv_body`
//! doesn't either), so an unmasked shift amount that happens to have bit 63
//! set can hit JS BigInt's negative-shift-count semantics, which no correctly
//! decoded RISC-V instruction stream can actually produce. That's narrow
//! enough, and specific enough to a pre-existing rendering quirk, that
//! reproducing it here isn't worth the risk of miscopying it.
//!
//! Similarly, `DivS32`/`RemS32`/`ShrS32` here sign-extend their 32-bit
//! operands with conventional two's-complement semantics (`a as u32 as
//! i32`) rather than the JS renderer's `signed(a&max32)`, which reinterprets
//! the *already-32-bit-masked* value as 64-bit-signed — a no-op for any
//! value below 2^63, so it never actually flips sign for a negative 32-bit
//! operand. That looks like a pre-existing bug in the JS path rather than
//! an intentional choice worth mirroring.

use alloc::{string::String, vec::Vec};
use vane_target_core::{FloatBinOp, FloatUnaryOp, FloatWidth, MemWidth, StackOp, StateSlot};

/// Host state a [`StackOp`] trace reads and writes as it interprets.
///
/// Implementors decide where register/memory state actually lives (e.g. a
/// local `Mem` — see `vane_arch::Mem` — for a fully server-owned run, or
/// something backed by a remote browser for select addresses/ecalls).
/// `interpret` never inspects state itself; every access goes through this
/// trait.
pub trait StackHost {
    fn load_reg(&mut self, r: u8) -> u64;
    fn store_reg(&mut self, r: u8, v: u64);
    /// Read an architecture-defined state slot. The default makes newly added
    /// slot-aware traces fail closed on legacy hosts rather than inheriting
    /// RISC-V's special register-zero semantics.
    fn load_state(&mut self, _slot: &StateSlot) -> u64 {
        0
    }
    /// Write an architecture-defined state slot. See [`Self::load_state`].
    fn store_state(&mut self, _slot: &StateSlot, _v: u64) {}
    fn load_mem(&mut self, addr: u64, width: MemWidth, signed: bool) -> u64;
    fn store_mem(&mut self, addr: u64, width: MemWidth, v: u64);
    /// A single byte at `addr`, used only by `StackOp::CheckCode`'s
    /// integrity check. Defaults to `load_mem(addr, MemWidth::B1, false) as u8`.
    fn byte(&mut self, addr: u64) -> u8 {
        self.load_mem(addr, MemWidth::B1, false) as u8
    }
    /// Called for `StackOp::Ecall`. Return `true` to keep interpreting the
    /// trace (the syscall was handled locally; execution falls through to
    /// the next op, matching `await $.ecall();` in the JS renderer, which
    /// never itself diverts control flow). Return `false` to stop
    /// interpreting and report [`Exit::Ecall`] to the caller (e.g. the
    /// `exit` syscall — see `Reactor::_ecall` in `crates/vane/src/lib.rs`).
    fn ecall(&mut self) -> bool;
    /// Called for `StackOp::Log`. Default: no-op.
    fn log(&mut self, _msg: &str) {}
}

/// Why [`interpret`] stopped.
#[derive(Debug, Clone)]
pub enum Exit {
    /// `StackOp::TailCall`: resume execution at this guest address (either a
    /// dynamic jump, or a `CheckCode` integrity-check miss re-dispatching to
    /// its own address — matching `return ()=>J(addr);` / `return
    /// J({addr}n);` in the JS renderer).
    TailCall(u64),
    /// `StackOp::Trap`: a decode error or `ebreak`.
    Trap(String),
    /// `StackOp::Ecall` and `StackHost::ecall` returned `false`.
    Ecall,
}

/// Interpret a fully materialized `StackOp` trace against `host`, returning
/// why it stopped.
fn float_unary(width: FloatWidth, op: FloatUnaryOp, value: u64) -> u64 {
    match width {
        FloatWidth::F32 => {
            let value = f32::from_bits(value as u32);
            let result = match op {
                FloatUnaryOp::Abs => value.abs(),
                FloatUnaryOp::Neg => -value,
                FloatUnaryOp::Sqrt => libm::sqrtf(value),
            };
            result.to_bits() as u64
        }
        FloatWidth::F64 => {
            let value = f64::from_bits(value);
            let result = match op {
                FloatUnaryOp::Abs => value.abs(),
                FloatUnaryOp::Neg => -value,
                FloatUnaryOp::Sqrt => libm::sqrt(value),
            };
            result.to_bits()
        }
    }
}

fn float_binary(width: FloatWidth, op: FloatBinOp, lhs: u64, rhs: u64) -> u64 {
    match width {
        FloatWidth::F32 => {
            let (lhs, rhs) = (f32::from_bits(lhs as u32), f32::from_bits(rhs as u32));
            let value = match op {
                FloatBinOp::Add => lhs + rhs,
                FloatBinOp::Sub => lhs - rhs,
                FloatBinOp::Mul => lhs * rhs,
                FloatBinOp::Div => lhs / rhs,
                FloatBinOp::Min => lhs.min(rhs),
                FloatBinOp::Max => lhs.max(rhs),
            };
            value.to_bits() as u64
        }
        FloatWidth::F64 => {
            let (lhs, rhs) = (f64::from_bits(lhs), f64::from_bits(rhs));
            let value = match op {
                FloatBinOp::Add => lhs + rhs,
                FloatBinOp::Sub => lhs - rhs,
                FloatBinOp::Mul => lhs * rhs,
                FloatBinOp::Div => lhs / rhs,
                FloatBinOp::Min => lhs.min(rhs),
                FloatBinOp::Max => lhs.max(rhs),
            };
            value.to_bits()
        }
    }
}

fn float_lt(width: FloatWidth, lhs: u64, rhs: u64) -> bool {
    match width {
        FloatWidth::F32 => f32::from_bits(lhs as u32) < f32::from_bits(rhs as u32),
        FloatWidth::F64 => f64::from_bits(lhs) < f64::from_bits(rhs),
    }
}
fn float_eq(width: FloatWidth, lhs: u64, rhs: u64) -> bool {
    match width {
        FloatWidth::F32 => f32::from_bits(lhs as u32) == f32::from_bits(rhs as u32),
        FloatWidth::F64 => f64::from_bits(lhs) == f64::from_bits(rhs),
    }
}
fn float_unordered(width: FloatWidth, lhs: u64, rhs: u64) -> bool {
    match width {
        FloatWidth::F32 => {
            f32::from_bits(lhs as u32).is_nan() || f32::from_bits(rhs as u32).is_nan()
        }
        FloatWidth::F64 => f64::from_bits(lhs).is_nan() || f64::from_bits(rhs).is_nan(),
    }
}

fn int_to_float(width: FloatWidth, signed: bool, int_width: u8, value: u64) -> u64 {
    let value = if signed {
        if int_width == 32 {
            value as u32 as i32 as i64 as f64
        } else {
            value as i64 as f64
        }
    } else if int_width == 32 {
        value as u32 as f64
    } else {
        value as f64
    };
    match width {
        FloatWidth::F32 => (value as f32).to_bits() as u64,
        FloatWidth::F64 => value.to_bits(),
    }
}
fn float_to_int(width: FloatWidth, signed: bool, int_width: u8, value: u64) -> u64 {
    let value = match width {
        FloatWidth::F32 => f32::from_bits(value as u32) as f64,
        FloatWidth::F64 => f64::from_bits(value),
    };
    if signed {
        if int_width == 32 {
            (value as i32) as i64 as u64
        } else {
            value as i64 as u64
        }
    } else if int_width == 32 {
        value as u32 as u64
    } else {
        value as u64
    }
}
fn float_convert(from: FloatWidth, to: FloatWidth, value: u64) -> u64 {
    match (from, to) {
        (FloatWidth::F32, FloatWidth::F32) | (FloatWidth::F64, FloatWidth::F64) => value,
        (FloatWidth::F32, FloatWidth::F64) => (f32::from_bits(value as u32) as f64).to_bits(),
        (FloatWidth::F64, FloatWidth::F32) => (f64::from_bits(value) as f32).to_bits() as u64,
    }
}

/// Interpret a fully materialized `StackOp` trace against `host`, returning
/// why it stopped.
///
/// # Panics
/// If `ops` is malformed (an `IfBegin`/`LoopBegin` without its matching
/// `IfElse`/`IfEnd`/`LoopEnd`, or a `Br` whose `relative_depth` exceeds the
/// number of currently open loops). `rv_emit`'s output is always
/// well-formed; this only fires on hand-built or otherwise-corrupt input.
pub fn interpret(ops: &[StackOp], host: &mut impl StackHost) -> Exit {
    let mut val: Vec<u64> = Vec::new();
    // Body-start index (one past `LoopBegin`) for each currently open loop,
    // innermost last. Used only to resolve `Br`'s jump target.
    let mut loop_stack: Vec<usize> = Vec::new();
    let mut ip = 0usize;

    macro_rules! pop {
        () => {
            val.pop().unwrap_or_default()
        };
    }
    macro_rules! pop2 {
        () => {{
            let b = pop!();
            let a = pop!();
            (a, b)
        }};
    }

    loop {
        let op = &ops[ip];
        match op {
            // ── Control flow ─────────────────────────────────────────────
            StackOp::LoopBegin { .. } => {
                loop_stack.push(ip + 1);
                ip += 1;
            }
            StackOp::LoopEnd => {
                loop_stack.pop();
                ip += 1;
            }
            StackOp::Br { relative_depth, .. } => {
                let idx = loop_stack.len() - *relative_depth as usize;
                let target = loop_stack[idx];
                loop_stack.truncate(idx + 1);
                ip = target;
            }
            StackOp::IfBegin => {
                let cond = pop!();
                if cond != 0 {
                    ip += 1;
                } else {
                    ip = find_if_else(ops, ip) + 1;
                }
            }
            StackOp::IfElse => {
                // Reached by falling through the end of a then-branch:
                // skip the else-branch entirely.
                ip = find_if_end(ops, ip) + 1;
            }
            StackOp::IfEnd => {
                ip += 1;
            }

            // ── Literals / registers ──────────────────────────────────────
            StackOp::PushImm(v) => {
                val.push(*v);
                ip += 1;
            }
            StackOp::LoadReg(r) => {
                val.push(if *r == 0 { 0 } else { host.load_reg(*r) });
                ip += 1;
            }
            StackOp::StoreReg(r) => {
                let v = pop!();
                if *r != 0 {
                    host.store_reg(*r, v);
                }
                ip += 1;
            }
            StackOp::Drop => {
                let _ = pop!();
                ip += 1;
            }
            StackOp::LoadState(slot) => {
                val.push(host.load_state(slot));
                ip += 1;
            }
            StackOp::StoreState(slot) => {
                host.store_state(slot, pop!());
                ip += 1;
            }

            // ── 64-bit arithmetic ─────────────────────────────────────────
            StackOp::Add64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_add(b));
                ip += 1;
            }
            StackOp::Sub64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_sub(b));
                ip += 1;
            }
            StackOp::Mul64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_mul(b));
                ip += 1;
            }
            StackOp::DivU64 => {
                let (a, b) = pop2!();
                val.push(if b == 0 { u64::MAX } else { a / b });
                ip += 1;
            }
            StackOp::DivS64 => {
                let (a, b) = pop2!();
                val.push(if b == 0 {
                    u64::MAX
                } else {
                    (a as i64).wrapping_div(b as i64) as u64
                });
                ip += 1;
            }
            StackOp::RemU64 => {
                let (a, b) = pop2!();
                // Matches render_stack_to_js: b==0 yields all-ones here too
                // (not the RISC-V-spec dividend), for cross-backend parity.
                val.push(if b == 0 { u64::MAX } else { a % b });
                ip += 1;
            }
            StackOp::RemS64 => {
                let (a, b) = pop2!();
                val.push(if b == 0 {
                    u64::MAX
                } else {
                    (a as i64).wrapping_rem(b as i64) as u64
                });
                ip += 1;
            }
            StackOp::And64 => {
                let (a, b) = pop2!();
                val.push(a & b);
                ip += 1;
            }
            StackOp::Or64 => {
                let (a, b) = pop2!();
                val.push(a | b);
                ip += 1;
            }
            StackOp::Xor64 => {
                let (a, b) = pop2!();
                val.push(a ^ b);
                ip += 1;
            }
            StackOp::Shl64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_shl((b & 63) as u32));
                ip += 1;
            }
            StackOp::ShrU64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_shr((b & 63) as u32));
                ip += 1;
            }
            StackOp::ShrS64 => {
                let (a, b) = pop2!();
                val.push(((a as i64).wrapping_shr((b & 63) as u32)) as u64);
                ip += 1;
            }
            StackOp::MulHU64 => {
                let (a, b) = pop2!();
                val.push(((a as u128 * b as u128) >> 64) as u64);
                ip += 1;
            }
            StackOp::MulHSU64 => {
                let (a, b) = pop2!();
                val.push((((a as i64 as i128) * (b as u128 as i128)) >> 64) as u64);
                ip += 1;
            }
            StackOp::MulHS64 => {
                let (a, b) = pop2!();
                val.push((((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64);
                ip += 1;
            }

            // ── 32-bit arithmetic (result sign-extended to i64) ───────────
            StackOp::Add32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_add(b as u32)));
                ip += 1;
            }
            StackOp::Sub32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_sub(b as u32)));
                ip += 1;
            }
            StackOp::Mul32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_mul(b as u32)));
                ip += 1;
            }
            StackOp::DivU32 => {
                let (a, b) = pop2!();
                let (a, b) = (a as u32, b as u32);
                val.push(if b == 0 { u64::MAX } else { sext32(a / b) });
                ip += 1;
            }
            StackOp::DivS32 => {
                let (a, b) = pop2!();
                let (a, b) = (a as u32 as i32, b as u32 as i32);
                val.push(if b == 0 {
                    u64::MAX
                } else {
                    sext32(a.wrapping_div(b) as u32)
                });
                ip += 1;
            }
            StackOp::RemU32 => {
                let (a, b) = pop2!();
                let (a, b) = (a as u32, b as u32);
                val.push(if b == 0 { u64::MAX } else { sext32(a % b) });
                ip += 1;
            }
            StackOp::RemS32 => {
                let (a, b) = pop2!();
                let (a, b) = (a as u32 as i32, b as u32 as i32);
                val.push(if b == 0 {
                    u64::MAX
                } else {
                    sext32(a.wrapping_rem(b) as u32)
                });
                ip += 1;
            }
            StackOp::Shl32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_shl(b as u32 & 31)));
                ip += 1;
            }
            StackOp::ShrU32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_shr(b as u32 & 31)));
                ip += 1;
            }
            StackOp::ShrS32 => {
                let (a, b) = pop2!();
                val.push(sext32(
                    ((a as u32 as i32).wrapping_shr(b as u32 & 31)) as u32,
                ));
                ip += 1;
            }

            // ── Conversions ───────────────────────────────────────────────
            StackOp::Trunc32 => {
                let a = pop!();
                val.push(a & 0xffff_ffff);
                ip += 1;
            }
            StackOp::SignExt32 => {
                let a = pop!();
                val.push(sext32(a as u32));
                ip += 1;
            }
            StackOp::AsSigned64 => {
                let a = pop!();
                val.push(a);
                ip += 1;
            } // no-op: representation is bit-identical
            StackOp::AsUnsigned64 => {
                let a = pop!();
                val.push(a);
                ip += 1;
            }

            // ── Comparisons (push 1 or 0) ──────────────────────────────────
            StackOp::Eq64 => {
                let (a, b) = pop2!();
                val.push((a == b) as u64);
                ip += 1;
            }
            StackOp::Ne64 => {
                let (a, b) = pop2!();
                val.push((a != b) as u64);
                ip += 1;
            }
            StackOp::LtU64 => {
                let (a, b) = pop2!();
                val.push((a < b) as u64);
                ip += 1;
            }
            StackOp::LtS64 => {
                let (a, b) = pop2!();
                val.push(((a as i64) < (b as i64)) as u64);
                ip += 1;
            }
            StackOp::GeU64 => {
                let (a, b) = pop2!();
                val.push((a >= b) as u64);
                ip += 1;
            }
            StackOp::GeS64 => {
                let (a, b) = pop2!();
                val.push(((a as i64) >= (b as i64)) as u64);
                ip += 1;
            }

            StackOp::FloatUnary { width, op } => {
                let a = pop!();
                val.push(float_unary(*width, *op, a));
                ip += 1;
            }
            StackOp::FloatBinary { width, op } => {
                let (a, b) = pop2!();
                val.push(float_binary(*width, *op, a, b));
                ip += 1;
            }
            StackOp::FloatLt { width } => {
                let (a, b) = pop2!();
                val.push(float_lt(*width, a, b) as u64);
                ip += 1;
            }
            StackOp::FloatEq { width } => {
                let (a, b) = pop2!();
                val.push(float_eq(*width, a, b) as u64);
                ip += 1;
            }
            StackOp::FloatUnordered { width } => {
                let (a, b) = pop2!();
                val.push(float_unordered(*width, a, b) as u64);
                ip += 1;
            }
            StackOp::IntToFloat {
                width,
                signed,
                int_width,
            } => {
                let value = pop!();
                val.push(int_to_float(*width, *signed, *int_width, value));
                ip += 1;
            }
            StackOp::FloatToInt {
                width,
                signed,
                int_width,
            } => {
                let value = pop!();
                val.push(float_to_int(*width, *signed, *int_width, value));
                ip += 1;
            }
            StackOp::FloatConvert { from, to } => {
                let value = pop!();
                val.push(float_convert(*from, *to, value));
                ip += 1;
            }

            // ── Memory ────────────────────────────────────────────────────
            StackOp::LoadMem { width, signed } => {
                let addr = pop!();
                val.push(host.load_mem(addr, *width, *signed));
                ip += 1;
            }
            StackOp::StoreMem { width } => {
                let v = pop!();
                let addr = pop!();
                host.store_mem(addr, *width, v);
                ip += 1;
            }

            // ── Side effects ──────────────────────────────────────────────
            StackOp::Ecall => {
                if host.ecall() {
                    ip += 1;
                } else {
                    return Exit::Ecall;
                }
            }
            StackOp::Trap(msg) => return Exit::Trap(msg.clone()),
            StackOp::TailCall => {
                let addr = pop!();
                return Exit::TailCall(addr);
            }
            StackOp::Log(msg) => {
                host.log(msg);
                ip += 1;
            }

            // ── Integrity check ─────────────────────────────────────────────
            StackOp::CheckCode {
                addr,
                expected,
                miss_label: _,
            } => {
                let matches = expected
                    .iter()
                    .enumerate()
                    .all(|(i, b)| host.byte(addr + i as u64) == *b);
                if matches {
                    ip += 1;
                } else {
                    return Exit::TailCall(*addr);
                }
            }
        }
    }
}

/// Async twin of [`StackHost`], for hosts whose register/memory state may
/// not be locally resident and need a chance to `.await` a round-trip
/// (e.g. a `vane-wsdom`-style host backed in part by a remote browser).
///
/// Implementations that never actually need to await anything (a purely
/// local host) can just return already-resolved values from these `async
/// fn`s — the executor driving [`interpret_async`] will never observe a
/// pending poll for them.
#[cfg(feature = "async-host")]
pub trait AsyncStackHost {
    async fn load_reg(&mut self, r: u8) -> u64;
    async fn store_reg(&mut self, r: u8, v: u64);
    async fn load_state(&mut self, _slot: &StateSlot) -> u64 {
        0
    }
    async fn store_state(&mut self, _slot: &StateSlot, _v: u64) {}
    async fn load_mem(&mut self, addr: u64, width: MemWidth, signed: bool) -> u64;
    async fn store_mem(&mut self, addr: u64, width: MemWidth, v: u64);
    /// See [`StackHost::byte`].
    async fn byte(&mut self, addr: u64) -> u8 {
        self.load_mem(addr, MemWidth::B1, false).await as u8
    }
    /// See [`StackHost::ecall`].
    async fn ecall(&mut self) -> bool;
    /// See [`StackHost::log`].
    async fn log(&mut self, _msg: &str) {}
}

#[cfg(feature = "async-host")]
/// Adapter from the generic `os-async` surfaces to vane's [`AsyncStackHost`].
///
/// This lets an interpreter back its register/memory state and ecall handling
/// onto the speet-neutral OS emulation surface instead of a custom host trait.
pub struct AsyncOsHost<'a, C: os_async::AsyncCtx + ?Sized, O: os_async::AsyncOS<C> + ?Sized> {
    pub ctx: &'a mut C,
    pub os: &'a mut O,
}

#[cfg(feature = "async-host")]
impl<'a, C: os_async::AsyncCtx + ?Sized, O: os_async::AsyncOS<C> + ?Sized> AsyncStackHost
    for AsyncOsHost<'a, C, O>
{
    async fn load_reg(&mut self, r: u8) -> u64 {
        let mut v = 0u64;
        self.ctx.reg(r, &mut v).await;
        v
    }

    async fn store_reg(&mut self, r: u8, v: u64) {
        let mut slot = v;
        self.ctx.set_reg(r, &mut slot).await;
    }

    async fn load_mem(&mut self, addr: u64, width: MemWidth, signed: bool) -> u64 {
        match width {
            MemWidth::B1 => {
                let mut v = 0u8;
                self.ctx.read(addr, &mut v).await;
                if signed {
                    v as i8 as i64 as u64
                } else {
                    v as u64
                }
            }
            MemWidth::B2 => {
                let mut v = 0u16;
                self.ctx.read(addr, &mut v).await;
                if signed {
                    v as i16 as i64 as u64
                } else {
                    v as u64
                }
            }
            MemWidth::B4 => {
                let mut v = 0u32;
                self.ctx.read(addr, &mut v).await;
                if signed {
                    v as i32 as i64 as u64
                } else {
                    v as u64
                }
            }
            MemWidth::B8 => {
                let mut v = 0u64;
                self.ctx.read(addr, &mut v).await;
                v
            }
        }
    }

    async fn store_mem(&mut self, addr: u64, width: MemWidth, v: u64) {
        match width {
            MemWidth::B1 => {
                let mut slot = v as u8;
                self.ctx.write(addr, &mut slot).await;
            }
            MemWidth::B2 => {
                let mut slot = v as u16;
                self.ctx.write(addr, &mut slot).await;
            }
            MemWidth::B4 => {
                let mut slot = v as u32;
                self.ctx.write(addr, &mut slot).await;
            }
            MemWidth::B8 => {
                let mut slot = v;
                self.ctx.write(addr, &mut slot).await;
            }
        }
    }

    async fn byte(&mut self, addr: u64) -> u8 {
        let mut v = 0u8;
        self.ctx.read(addr, &mut v).await;
        v
    }

    async fn ecall(&mut self) -> bool {
        // Default: treat any OS result as "continue interpreting".
        let _ = self.os.syscall(self.ctx).await;
        true
    }
}

/// Async twin of [`interpret`] — identical control-flow/arithmetic logic,
/// `.await`ing every [`AsyncStackHost`] call instead of calling [`StackHost`]
/// synchronously. Kept as a separate function (rather than one generic over
/// sync/async) to match how `render_stack_to_js`/`render_stack_to_wasm`
/// (`crate::template`) are already two independent lowerings rather than a
/// single parameterized one.
#[cfg(feature = "async-host")]
pub async fn interpret_async(ops: &[StackOp], host: &mut impl AsyncStackHost) -> Exit {
    let mut val: Vec<u64> = Vec::new();
    let mut loop_stack: Vec<usize> = Vec::new();
    let mut ip = 0usize;

    macro_rules! pop {
        () => {
            val.pop().unwrap_or_default()
        };
    }
    macro_rules! pop2 {
        () => {{
            let b = pop!();
            let a = pop!();
            (a, b)
        }};
    }

    loop {
        let op = &ops[ip];
        match op {
            StackOp::LoopBegin { .. } => {
                loop_stack.push(ip + 1);
                ip += 1;
            }
            StackOp::LoopEnd => {
                loop_stack.pop();
                ip += 1;
            }
            StackOp::Br { relative_depth, .. } => {
                let idx = loop_stack.len() - *relative_depth as usize;
                let target = loop_stack[idx];
                loop_stack.truncate(idx + 1);
                ip = target;
            }
            StackOp::IfBegin => {
                let cond = pop!();
                if cond != 0 {
                    ip += 1;
                } else {
                    ip = find_if_else(ops, ip) + 1;
                }
            }
            StackOp::IfElse => {
                ip = find_if_end(ops, ip) + 1;
            }
            StackOp::IfEnd => {
                ip += 1;
            }
            StackOp::PushImm(v) => {
                val.push(*v);
                ip += 1;
            }
            StackOp::LoadReg(r) => {
                val.push(if *r == 0 { 0 } else { host.load_reg(*r).await });
                ip += 1;
            }
            StackOp::StoreReg(r) => {
                let v = pop!();
                if *r != 0 {
                    host.store_reg(*r, v).await;
                }
                ip += 1;
            }
            StackOp::Drop => {
                let _ = pop!();
                ip += 1;
            }
            StackOp::LoadState(slot) => {
                val.push(host.load_state(slot).await);
                ip += 1;
            }
            StackOp::StoreState(slot) => {
                host.store_state(slot, pop!()).await;
                ip += 1;
            }
            StackOp::Add64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_add(b));
                ip += 1;
            }
            StackOp::Sub64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_sub(b));
                ip += 1;
            }
            StackOp::Mul64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_mul(b));
                ip += 1;
            }
            StackOp::DivU64 => {
                let (a, b) = pop2!();
                val.push(if b == 0 { u64::MAX } else { a / b });
                ip += 1;
            }
            StackOp::DivS64 => {
                let (a, b) = pop2!();
                val.push(if b == 0 {
                    u64::MAX
                } else {
                    (a as i64).wrapping_div(b as i64) as u64
                });
                ip += 1;
            }
            StackOp::RemU64 => {
                let (a, b) = pop2!();
                val.push(if b == 0 { u64::MAX } else { a % b });
                ip += 1;
            }
            StackOp::RemS64 => {
                let (a, b) = pop2!();
                val.push(if b == 0 {
                    u64::MAX
                } else {
                    (a as i64).wrapping_rem(b as i64) as u64
                });
                ip += 1;
            }
            StackOp::And64 => {
                let (a, b) = pop2!();
                val.push(a & b);
                ip += 1;
            }
            StackOp::Or64 => {
                let (a, b) = pop2!();
                val.push(a | b);
                ip += 1;
            }
            StackOp::Xor64 => {
                let (a, b) = pop2!();
                val.push(a ^ b);
                ip += 1;
            }
            StackOp::Shl64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_shl((b & 63) as u32));
                ip += 1;
            }
            StackOp::ShrU64 => {
                let (a, b) = pop2!();
                val.push(a.wrapping_shr((b & 63) as u32));
                ip += 1;
            }
            StackOp::ShrS64 => {
                let (a, b) = pop2!();
                val.push(((a as i64).wrapping_shr((b & 63) as u32)) as u64);
                ip += 1;
            }
            StackOp::MulHU64 => {
                let (a, b) = pop2!();
                val.push(((a as u128 * b as u128) >> 64) as u64);
                ip += 1;
            }
            StackOp::MulHSU64 => {
                let (a, b) = pop2!();
                val.push((((a as i64 as i128) * (b as u128 as i128)) >> 64) as u64);
                ip += 1;
            }
            StackOp::MulHS64 => {
                let (a, b) = pop2!();
                val.push((((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64);
                ip += 1;
            }
            StackOp::Add32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_add(b as u32)));
                ip += 1;
            }
            StackOp::Sub32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_sub(b as u32)));
                ip += 1;
            }
            StackOp::Mul32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_mul(b as u32)));
                ip += 1;
            }
            StackOp::DivU32 => {
                let (a, b) = pop2!();
                let (a, b) = (a as u32, b as u32);
                val.push(if b == 0 { u64::MAX } else { sext32(a / b) });
                ip += 1;
            }
            StackOp::DivS32 => {
                let (a, b) = pop2!();
                let (a, b) = (a as u32 as i32, b as u32 as i32);
                val.push(if b == 0 {
                    u64::MAX
                } else {
                    sext32(a.wrapping_div(b) as u32)
                });
                ip += 1;
            }
            StackOp::RemU32 => {
                let (a, b) = pop2!();
                let (a, b) = (a as u32, b as u32);
                val.push(if b == 0 { u64::MAX } else { sext32(a % b) });
                ip += 1;
            }
            StackOp::RemS32 => {
                let (a, b) = pop2!();
                let (a, b) = (a as u32 as i32, b as u32 as i32);
                val.push(if b == 0 {
                    u64::MAX
                } else {
                    sext32(a.wrapping_rem(b) as u32)
                });
                ip += 1;
            }
            StackOp::Shl32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_shl(b as u32 & 31)));
                ip += 1;
            }
            StackOp::ShrU32 => {
                let (a, b) = pop2!();
                val.push(sext32((a as u32).wrapping_shr(b as u32 & 31)));
                ip += 1;
            }
            StackOp::ShrS32 => {
                let (a, b) = pop2!();
                val.push(sext32(
                    ((a as u32 as i32).wrapping_shr(b as u32 & 31)) as u32,
                ));
                ip += 1;
            }
            StackOp::Trunc32 => {
                let a = pop!();
                val.push(a & 0xffff_ffff);
                ip += 1;
            }
            StackOp::SignExt32 => {
                let a = pop!();
                val.push(sext32(a as u32));
                ip += 1;
            }
            StackOp::AsSigned64 => {
                let a = pop!();
                val.push(a);
                ip += 1;
            }
            StackOp::AsUnsigned64 => {
                let a = pop!();
                val.push(a);
                ip += 1;
            }
            StackOp::Eq64 => {
                let (a, b) = pop2!();
                val.push((a == b) as u64);
                ip += 1;
            }
            StackOp::Ne64 => {
                let (a, b) = pop2!();
                val.push((a != b) as u64);
                ip += 1;
            }
            StackOp::LtU64 => {
                let (a, b) = pop2!();
                val.push((a < b) as u64);
                ip += 1;
            }
            StackOp::LtS64 => {
                let (a, b) = pop2!();
                val.push(((a as i64) < (b as i64)) as u64);
                ip += 1;
            }
            StackOp::GeU64 => {
                let (a, b) = pop2!();
                val.push((a >= b) as u64);
                ip += 1;
            }
            StackOp::GeS64 => {
                let (a, b) = pop2!();
                val.push(((a as i64) >= (b as i64)) as u64);
                ip += 1;
            }
            StackOp::FloatUnary { width, op } => {
                let value = pop!();
                val.push(float_unary(*width, *op, value));
                ip += 1;
            }
            StackOp::FloatBinary { width, op } => {
                let (a, b) = pop2!();
                val.push(float_binary(*width, *op, a, b));
                ip += 1;
            }
            StackOp::FloatLt { width } => {
                let (a, b) = pop2!();
                val.push(float_lt(*width, a, b) as u64);
                ip += 1;
            }
            StackOp::FloatEq { width } => {
                let (a, b) = pop2!();
                val.push(float_eq(*width, a, b) as u64);
                ip += 1;
            }
            StackOp::FloatUnordered { width } => {
                let (a, b) = pop2!();
                val.push(float_unordered(*width, a, b) as u64);
                ip += 1;
            }
            StackOp::IntToFloat {
                width,
                signed,
                int_width,
            } => {
                let value = pop!();
                val.push(int_to_float(*width, *signed, *int_width, value));
                ip += 1;
            }
            StackOp::FloatToInt {
                width,
                signed,
                int_width,
            } => {
                let value = pop!();
                val.push(float_to_int(*width, *signed, *int_width, value));
                ip += 1;
            }
            StackOp::FloatConvert { from, to } => {
                let value = pop!();
                val.push(float_convert(*from, *to, value));
                ip += 1;
            }
            StackOp::LoadMem { width, signed } => {
                let addr = pop!();
                val.push(host.load_mem(addr, *width, *signed).await);
                ip += 1;
            }
            StackOp::StoreMem { width } => {
                let v = pop!();
                let addr = pop!();
                host.store_mem(addr, *width, v).await;
                ip += 1;
            }
            StackOp::Ecall => {
                if host.ecall().await {
                    ip += 1;
                } else {
                    return Exit::Ecall;
                }
            }
            StackOp::Trap(msg) => return Exit::Trap(msg.clone()),
            StackOp::TailCall => {
                let addr = pop!();
                return Exit::TailCall(addr);
            }
            StackOp::Log(msg) => {
                host.log(msg).await;
                ip += 1;
            }
            StackOp::CheckCode {
                addr,
                expected,
                miss_label: _,
            } => {
                let mut matches = true;
                for (i, b) in expected.iter().enumerate() {
                    if host.byte(addr + i as u64).await != *b {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    ip += 1;
                } else {
                    return Exit::TailCall(*addr);
                }
            }
        }
    }
}

fn sext32(v: u32) -> u64 {
    v as i32 as i64 as u64
}

/// From an `IfBegin` at `idx`, find the index of its matching `IfElse`
/// (same nesting level).
fn find_if_else(ops: &[StackOp], idx: usize) -> usize {
    let mut depth = 0i32;
    let mut i = idx + 1;
    loop {
        match &ops[i] {
            StackOp::IfBegin => depth += 1,
            StackOp::IfElse if depth == 0 => return i,
            StackOp::IfEnd => depth -= 1,
            _ => {}
        }
        i += 1;
    }
}

/// From an `IfElse` at `idx`, find the index of its matching `IfEnd`.
fn find_if_end(ops: &[StackOp], idx: usize) -> usize {
    let mut depth = 0i32;
    let mut i = idx + 1;
    loop {
        match &ops[i] {
            StackOp::IfBegin => depth += 1,
            StackOp::IfEnd if depth == 0 => return i,
            StackOp::IfEnd => depth -= 1,
            _ => {}
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Local register/memory host, no delegation — the reference impl for a
    /// fully server-owned run.
    struct TestHost {
        regs: [u64; 32],
        mem: alloc::collections::BTreeMap<u64, u8>,
        ecalls: Vec<u64>,
        logs: Vec<String>,
    }
    impl TestHost {
        fn new() -> Self {
            Self {
                regs: [0; 32],
                mem: Default::default(),
                ecalls: Vec::new(),
                logs: Vec::new(),
            }
        }
    }
    impl StackHost for TestHost {
        fn load_reg(&mut self, r: u8) -> u64 {
            self.regs[r as usize]
        }
        fn store_reg(&mut self, r: u8, v: u64) {
            self.regs[r as usize] = v;
        }
        fn load_mem(&mut self, addr: u64, width: MemWidth, signed: bool) -> u64 {
            let n = match width {
                MemWidth::B1 => 1,
                MemWidth::B2 => 2,
                MemWidth::B4 => 4,
                MemWidth::B8 => 8,
            };
            let mut bytes = [0u8; 8];
            for i in 0..n {
                bytes[i] = *self.mem.get(&(addr + i as u64)).unwrap_or(&0);
            }
            let u = u64::from_le_bytes(bytes);
            if signed && n < 8 {
                let shift = (8 - n) * 8;
                (((u << shift) as i64) >> shift) as u64
            } else {
                u
            }
        }
        fn store_mem(&mut self, addr: u64, width: MemWidth, v: u64) {
            let n = match width {
                MemWidth::B1 => 1,
                MemWidth::B2 => 2,
                MemWidth::B4 => 4,
                MemWidth::B8 => 8,
            };
            for (i, b) in v.to_le_bytes().into_iter().take(n).enumerate() {
                self.mem.insert(addr + i as u64, b);
            }
        }
        fn byte(&mut self, addr: u64) -> u8 {
            *self.mem.get(&addr).unwrap_or(&0)
        }
        fn ecall(&mut self) -> bool {
            self.ecalls.push(self.regs[17]); // a7
            self.regs[17] != 93
        }
        fn log(&mut self, msg: &str) {
            self.logs.push(msg.into());
        }
    }

    #[test]
    fn straight_line_arithmetic() {
        // x3 = x1 + x2
        let ops = vec![
            StackOp::LoadReg(1),
            StackOp::LoadReg(2),
            StackOp::Add64,
            StackOp::StoreReg(3),
            StackOp::PushImm(7),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        host.regs[1] = 10;
        host.regs[2] = 32;
        let exit = interpret(&ops, &mut host);
        assert_eq!(host.regs[3], 42);
        assert!(matches!(exit, Exit::TailCall(7)));
    }

    #[test]
    fn if_else_picks_the_taken_branch() {
        // if (x1 != 0) { x2 = 1 } else { x2 = 2 }
        let taken = vec![
            StackOp::LoadReg(1),
            StackOp::IfBegin,
            StackOp::PushImm(1),
            StackOp::StoreReg(2),
            StackOp::IfElse,
            StackOp::PushImm(2),
            StackOp::StoreReg(2),
            StackOp::IfEnd,
            StackOp::PushImm(0),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        host.regs[1] = 1;
        interpret(&taken, &mut host);
        assert_eq!(host.regs[2], 1);

        let mut host = TestHost::new();
        host.regs[1] = 0;
        interpret(&taken, &mut host);
        assert_eq!(host.regs[2], 2);
    }

    #[test]
    fn br_loops_back_to_loop_start() {
        // x1 counts down from 3 to 0 via a backward Br, then tail-calls.
        let ops = vec![
            StackOp::LoopBegin { label: "L".into() },
            StackOp::LoadReg(1),
            StackOp::PushImm(1),
            StackOp::Sub64,
            StackOp::StoreReg(1),
            StackOp::LoadReg(1),
            StackOp::IfBegin,
            StackOp::Br {
                relative_depth: 1,
                label: "L".into(),
            },
            StackOp::IfElse,
            StackOp::IfEnd,
            StackOp::LoopEnd,
            StackOp::PushImm(99),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        host.regs[1] = 3;
        let exit = interpret(&ops, &mut host);
        assert_eq!(host.regs[1], 0);
        assert!(matches!(exit, Exit::TailCall(99)));
    }

    #[test]
    fn ecall_exit_stops_interpretation() {
        let ops = vec![
            StackOp::PushImm(93),
            StackOp::StoreReg(17), // a7 = 93 (exit)
            StackOp::Ecall,
            StackOp::PushImm(0xdead),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        let exit = interpret(&ops, &mut host);
        assert!(matches!(exit, Exit::Ecall));
        assert_eq!(host.ecalls, vec![93]);
    }

    #[test]
    fn ecall_continue_falls_through() {
        let ops = vec![
            StackOp::PushImm(1),
            StackOp::StoreReg(17), // a7 = 1 (not exit)
            StackOp::Ecall,
            StackOp::PushImm(0xbeef),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        let exit = interpret(&ops, &mut host);
        assert!(matches!(exit, Exit::TailCall(0xbeef)));
    }

    #[test]
    fn load_store_mem_round_trip() {
        let ops = vec![
            StackOp::PushImm(0x100), // addr
            StackOp::PushImm(0x1234),
            StackOp::StoreMem {
                width: MemWidth::B2,
            },
            StackOp::PushImm(0x100),
            StackOp::LoadMem {
                width: MemWidth::B2,
                signed: false,
            },
            StackOp::StoreReg(5),
            StackOp::PushImm(0),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        interpret(&ops, &mut host);
        assert_eq!(host.regs[5], 0x1234);
    }

    #[test]
    fn check_code_mismatch_tailcalls_to_addr() {
        let ops = vec![
            StackOp::CheckCode {
                addr: 0x1000,
                expected: alloc::boxed::Box::from([0xAAu8, 0xBB, 0xCC, 0xDD]),
                miss_label: "x".into(),
            },
            StackOp::PushImm(0),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new(); // memory at 0x1000 is all zero -> mismatch
        let exit = interpret(&ops, &mut host);
        assert!(matches!(exit, Exit::TailCall(0x1000)));
    }

    #[test]
    fn check_code_match_falls_through() {
        let expected = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let ops = vec![
            StackOp::CheckCode {
                addr: 0x1000,
                expected: alloc::boxed::Box::from(expected),
                miss_label: "x".into(),
            },
            StackOp::PushImm(0x2000),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        for (i, b) in expected.into_iter().enumerate() {
            host.mem.insert(0x1000 + i as u64, b);
        }
        let exit = interpret(&ops, &mut host);
        assert!(matches!(exit, Exit::TailCall(0x2000)));
    }

    #[test]
    fn div_and_rem_by_zero_match_js_renderer() {
        let ops = vec![
            StackOp::PushImm(5),
            StackOp::PushImm(0),
            StackOp::DivU64,
            StackOp::StoreReg(1),
            StackOp::PushImm(5),
            StackOp::PushImm(0),
            StackOp::RemU64,
            StackOp::StoreReg(2),
            StackOp::PushImm(0),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        interpret(&ops, &mut host);
        assert_eq!(host.regs[1], u64::MAX);
        assert_eq!(host.regs[2], u64::MAX);
    }

    #[test]
    fn signed_div_min_by_neg_one_wraps_like_bigint() {
        let ops = vec![
            StackOp::PushImm(i64::MIN as u64),
            StackOp::PushImm(-1i64 as u64),
            StackOp::DivS64,
            StackOp::StoreReg(1),
            StackOp::PushImm(0),
            StackOp::TailCall,
        ];
        let mut host = TestHost::new();
        interpret(&ops, &mut host);
        assert_eq!(host.regs[1], i64::MIN as u64);
    }
}

#[cfg(all(test, feature = "async-host"))]
mod async_tests {
    use super::*;
    use alloc::vec;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    /// Same state as `tests::TestHost`, but every op resolves immediately —
    /// exercises `interpret_async`'s control flow without needing a real
    /// executor to observe a pending poll.
    struct AsyncTestHost {
        regs: [u64; 32],
        mem: alloc::collections::BTreeMap<u64, u8>,
    }
    impl AsyncTestHost {
        fn new() -> Self {
            Self {
                regs: [0; 32],
                mem: Default::default(),
            }
        }
    }
    impl AsyncStackHost for AsyncTestHost {
        async fn load_reg(&mut self, r: u8) -> u64 {
            self.regs[r as usize]
        }
        async fn store_reg(&mut self, r: u8, v: u64) {
            self.regs[r as usize] = v;
        }
        async fn load_mem(&mut self, addr: u64, width: MemWidth, _signed: bool) -> u64 {
            let n = match width {
                MemWidth::B1 => 1,
                MemWidth::B2 => 2,
                MemWidth::B4 => 4,
                MemWidth::B8 => 8,
            };
            let mut bytes = [0u8; 8];
            for i in 0..n {
                bytes[i] = *self.mem.get(&(addr + i as u64)).unwrap_or(&0);
            }
            u64::from_le_bytes(bytes)
        }
        async fn store_mem(&mut self, addr: u64, width: MemWidth, v: u64) {
            let n = match width {
                MemWidth::B1 => 1,
                MemWidth::B2 => 2,
                MemWidth::B4 => 4,
                MemWidth::B8 => 8,
            };
            for (i, b) in v.to_le_bytes().into_iter().take(n).enumerate() {
                self.mem.insert(addr + i as u64, b);
            }
        }
        async fn ecall(&mut self) -> bool {
            self.regs[17] != 93
        }
    }

    fn noop_waker() -> Waker {
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    /// Every `AsyncTestHost` future above resolves on its first poll, so a
    /// single-poll busy loop (no real executor needed) is enough to drive
    /// `interpret_async` to completion in a test.
    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    #[test]
    fn scalar_float_ops_preserve_width_and_nan_ordering() {
        let ops = vec![
            StackOp::PushImm(1.25f32.to_bits() as u64),
            StackOp::PushImm(2.5f32.to_bits() as u64),
            StackOp::FloatBinary {
                width: FloatWidth::F32,
                op: FloatBinOp::Add,
            },
            StackOp::StoreState(StateSlot::new("fp32")),
            StackOp::PushImm(f64::NAN.to_bits()),
            StackOp::PushImm(1.0f64.to_bits()),
            StackOp::FloatEq {
                width: FloatWidth::F64,
            },
            StackOp::StoreState(StateSlot::new("nan_eq")),
            StackOp::PushImm(4.0f64.to_bits()),
            StackOp::FloatUnary {
                width: FloatWidth::F64,
                op: FloatUnaryOp::Sqrt,
            },
            StackOp::StoreState(StateSlot::new("sqrt")),
            StackOp::PushImm(0),
            StackOp::TailCall,
        ];
        struct Host {
            values: alloc::collections::BTreeMap<StateSlot, u64>,
        }
        impl StackHost for Host {
            fn load_reg(&mut self, _: u8) -> u64 {
                0
            }
            fn store_reg(&mut self, _: u8, _: u64) {}
            fn load_state(&mut self, slot: &StateSlot) -> u64 {
                self.values.get(slot).copied().unwrap_or(0)
            }
            fn store_state(&mut self, slot: &StateSlot, value: u64) {
                self.values.insert(slot.clone(), value);
            }
            fn load_mem(&mut self, _: u64, _: MemWidth, _: bool) -> u64 {
                0
            }
            fn store_mem(&mut self, _: u64, _: MemWidth, _: u64) {}
            fn ecall(&mut self) -> bool {
                false
            }
        }
        let mut host = Host {
            values: Default::default(),
        };
        assert!(matches!(interpret(&ops, &mut host), Exit::TailCall(0)));
        assert_eq!(
            host.values[&StateSlot::new("fp32")],
            3.75f32.to_bits() as u64
        );
        assert_eq!(host.values[&StateSlot::new("nan_eq")], 0);
        assert_eq!(host.values[&StateSlot::new("sqrt")], 2.0f64.to_bits());
    }

    #[test]
    fn matches_sync_interpreter_for_straight_line_arithmetic() {
        let ops = vec![
            StackOp::LoadReg(1),
            StackOp::LoadReg(2),
            StackOp::Add64,
            StackOp::StoreReg(3),
            StackOp::PushImm(7),
            StackOp::TailCall,
        ];
        let mut host = AsyncTestHost::new();
        host.regs[1] = 10;
        host.regs[2] = 32;
        let exit = block_on(interpret_async(&ops, &mut host));
        assert_eq!(host.regs[3], 42);
        assert!(matches!(exit, Exit::TailCall(7)));
    }

    #[test]
    fn ecall_exit_stops_interpretation() {
        let ops = vec![
            StackOp::PushImm(93),
            StackOp::StoreReg(17),
            StackOp::Ecall,
            StackOp::PushImm(0xdead),
            StackOp::TailCall,
        ];
        let mut host = AsyncTestHost::new();
        let exit = block_on(interpret_async(&ops, &mut host));
        assert!(matches!(exit, Exit::Ecall));
    }
}
