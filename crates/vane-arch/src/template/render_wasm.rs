//! `render_stack_to_wasm`: `StackOp` stream → WASM opcodes.
//!
//! Lives in its own module file because it is written with unstable
//! `gen`-block syntax: a `#[cfg]` on the *item* cannot hide a parse-level
//! feature gate (files are parsed whole), but a cfg-gated `mod` prevents
//! the file from being read at all — so interpreter-only, stable-toolchain
//! consumers (spday-exec) can build vane-arch with `target-wasm` off.

use vane_target_core::{MemWidth, StackOp};
use vane_target_wasm::{JitOpcode, WasmAbiConfig};
use wasmparser::Operator;

/// Render a `StackOp` stream to a sequence of WASM opcodes.
///
/// Registers map to WASM params/locals (`local_index = abi.reg_base + register_number`),
/// matching speet's `OobConfig` shared calling convention — see
/// [`WasmAbiConfig`]'s doc comment. `Br` depths are adjusted for WASM: StackOp
/// `relative_depth` counts *loop* boundaries, while WASM `br` counts all open
/// blocks (loops + if-blocks). The renderer tracks open if-blocks and adjusts
/// accordingly.
///
/// `LoadState`/`StoreState` (named architectural state — e.g. AArch64's
/// GPRs/NZCV/V-registers) lower via `abi.state_slots`; a name absent from
/// that table still lowers to `Unreachable`. `Float*` ops remain
/// `unreachable` — deferred, not part of this pass's scope (the first
/// JIT-targeted corpus slice is integer-only). `MulHU64`/`MulHSU64`/`MulHS64`
/// (64x64->128 high multiplies) likewise remain `unreachable`, needing
/// local-variable-heavy lowering not attempted here.
#[cfg(feature = "target-wasm")]
pub fn render_stack_to_wasm<'a>(
    ops: impl Iterator<Item = StackOp> + 'a,
    abi: WasmAbiConfig,
) -> impl Iterator<Item = JitOpcode<'a>> + 'a {
    gen move {
        let mut if_depth = 0u32;
        // ── Local helpers for multi-instruction patterns ────────────────
        macro_rules! op {
            ($o:expr) => {
                yield JitOpcode::Operator { op: $o }
            };
        }
        macro_rules! mem_arg {
            ($max_align:expr) => {
                wasmparser::MemArg {
                    align: 0,
                    max_align: $max_align,
                    offset: 0,
                    memory: abi.mem_idx,
                }
            };
        }
        // Div/rem-by-zero: WASM's native div/rem instructions trap on a
        // zero divisor, so both renderers emit an explicit zero check. The
        // zero-branch value follows the RISC-V M extension table: all-ones
        // for *div*, the *dividend* for *rem* (the old all-ones-for-both
        // convention mirrored a JS-renderer quirk; corrected against the
        // ISA after the cross-fuzz differential flagged it — see
        // CROSS_FUZZ_PLAN.md M2).
        macro_rules! yield_div_rem_64 {
            // `$zero_branch`: the WASM instruction producing the zero-
            // divisor result (I64Const -1 for div; LocalGet a for rem).
            ($wasm_op:expr, $zero_branch:expr) => {{
                op!(Operator::LocalSet {
                    local_index: abi.scratch_b
                }); // pop b
                op!(Operator::LocalSet {
                    local_index: abi.scratch_a
                }); // pop a
                op!(Operator::LocalGet {
                    local_index: abi.scratch_b
                });
                op!(Operator::I64Eqz);
                op!(Operator::If {
                    blockty: wasmparser::BlockType::Type(wasmparser::ValType::I64)
                });
                $zero_branch;
                op!(Operator::Else);
                op!(Operator::LocalGet {
                    local_index: abi.scratch_a
                });
                op!(Operator::LocalGet {
                    local_index: abi.scratch_b
                });
                op!($wasm_op);
                op!(Operator::End);
            }};
        }
        // Same, but for the 32-bit ops: the zero check and the operation
        // itself both act on the operands' low 32 bits (RISC-V `*W`
        // instructions operate on a 32-bit view regardless of upper bits).
        macro_rules! yield_div_rem_32 {
            ($wasm_op:expr, $zero_branch:expr) => {{
                op!(Operator::LocalSet {
                    local_index: abi.scratch_b
                }); // pop b:i64
                op!(Operator::LocalSet {
                    local_index: abi.scratch_a
                }); // pop a:i64
                op!(Operator::LocalGet {
                    local_index: abi.scratch_b
                });
                op!(Operator::I32WrapI64);
                op!(Operator::I32Eqz);
                op!(Operator::If {
                    blockty: wasmparser::BlockType::Type(wasmparser::ValType::I64)
                });
                $zero_branch;
                op!(Operator::Else);
                op!(Operator::LocalGet {
                    local_index: abi.scratch_a
                });
                op!(Operator::I32WrapI64);
                op!(Operator::LocalGet {
                    local_index: abi.scratch_b
                });
                op!(Operator::I32WrapI64);
                op!($wasm_op);
                op!(Operator::I64ExtendI32S);
                op!(Operator::End);
            }};
        }
        // Binary 32-bit ops with no zero-divisor concern: wrap both operands
        // to i32 (reordering through `scratch_b` since WASM can only wrap
        // the top-of-stack value), operate, then sign-extend the result back
        // to i64 ("32-bit arithmetic, result sign-extended to i64" per
        // `StackOp`'s doc).
        macro_rules! yield_binop32 {
            ($wasm_op:expr) => {{
                op!(Operator::LocalSet {
                    local_index: abi.scratch_b
                }); // pop b:i64
                op!(Operator::I32WrapI64); // wrap a -> i32
                op!(Operator::LocalGet {
                    local_index: abi.scratch_b
                });
                op!(Operator::I32WrapI64); // wrap b -> i32
                op!($wasm_op);
                op!(Operator::I64ExtendI32S);
            }};
        }
        for op in ops {
            match op {
                // ── Control flow ──────────────────────────────────────────
                StackOp::LoopBegin { .. } => {
                    yield JitOpcode::Operator {
                        op: Operator::Loop {
                            blockty: wasmparser::BlockType::Empty,
                        },
                    };
                }
                StackOp::LoopEnd => {
                    yield JitOpcode::Operator { op: Operator::End };
                }
                StackOp::Br { relative_depth, .. } => {
                    // StackOp rel_depth counts loop boundaries only; WASM br
                    // counts all open blocks.  Subtract 1 (br 0 = current loop
                    // in StackOp means rel_depth 1), then add if_depth.
                    let wasm_depth = (relative_depth - 1) + if_depth;
                    yield JitOpcode::Operator {
                        op: Operator::Br {
                            relative_depth: wasm_depth,
                        },
                    };
                }
                StackOp::IfBegin => {
                    // Convert i64 condition (0=false) to i32 for WASM if.
                    yield JitOpcode::Operator {
                        op: Operator::I64Const { value: 0 },
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64Ne,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::If {
                            blockty: wasmparser::BlockType::Empty,
                        },
                    };
                    if_depth += 1;
                }
                StackOp::IfElse => {
                    yield JitOpcode::Operator { op: Operator::Else };
                }
                StackOp::IfEnd => {
                    if_depth -= 1;
                    yield JitOpcode::Operator { op: Operator::End };
                }

                // ── Literals / registers ──────────────────────────────────
                StackOp::PushImm(v) => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Const { value: v as i64 },
                    };
                }
                StackOp::LoadReg(r) => {
                    // Register 0 always pushes 0 (RISC-V x0 semantics) — the
                    // corresponding local is *not* trusted to hold 0, since
                    // it is a real function parameter a caller could pass
                    // anything into, unlike a persistent global that's never
                    // written.
                    if r == 0 {
                        op!(Operator::I64Const { value: 0 });
                    } else {
                        op!(Operator::LocalGet {
                            local_index: abi.reg_base + r as u32
                        });
                    }
                }
                StackOp::StoreReg(r) => {
                    if r == 0 {
                        op!(Operator::Drop);
                    } else {
                        op!(Operator::LocalSet {
                            local_index: abi.reg_base + r as u32
                        });
                    }
                }
                StackOp::Drop => {
                    yield JitOpcode::Operator { op: Operator::Drop };
                }
                // Named state (AArch64's GPRs/NZCV/V-registers, addressed by
                // name rather than an index like RISC-V's LoadReg/StoreReg)
                // maps to a local via `abi.state_slots`, a small table built
                // once per ISA crate at Rust compile time (see
                // `WasmAbiConfig::state_slots`'s doc comment) — a linear
                // scan, since the table is tiny and this avoids a
                // sorted-invariant bug class. A name not in the table (an
                // architecture that hasn't wired this slot yet) still lowers
                // to `Unreachable`: an unmapped slot name is a genuine bug,
                // not something to silently zero, so this preserves the
                // original safety property for anything not yet mapped.
                StackOp::LoadState(slot) => {
                    match abi
                        .state_slots
                        .iter()
                        .find(|(name, _)| *name == slot.as_str())
                    {
                        Some((_, local)) => op!(Operator::LocalGet {
                            local_index: *local
                        }),
                        None => op!(Operator::Unreachable),
                    }
                }
                StackOp::StoreState(slot) => {
                    match abi
                        .state_slots
                        .iter()
                        .find(|(name, _)| *name == slot.as_str())
                    {
                        Some((_, local)) => op!(Operator::LocalSet {
                            local_index: *local
                        }),
                        None => op!(Operator::Unreachable),
                    }
                }

                // ── 64-bit arithmetic ─────────────────────────────────────
                StackOp::Add64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Add,
                    };
                }
                StackOp::Sub64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Sub,
                    };
                }
                StackOp::Mul64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Mul,
                    };
                }
                // Div/Rem all need an explicit zero-divisor check: WASM's
                // native div/rem instructions *trap* on a zero divisor, but
                // RISC-V semantics define a result instead (quotient =
                // all-ones, remainder = the dividend unchanged) — matching
                // `emit_div_rem_by_zero_checked` below.
                StackOp::DivU64 => {
                    yield_div_rem_64!(Operator::I64DivU, op!(Operator::I64Const { value: -1 }));
                }
                StackOp::DivS64 => {
                    yield_div_rem_64!(Operator::I64DivS, op!(Operator::I64Const { value: -1 }));
                }
                StackOp::RemU64 => {
                    // Remainder-by-zero: the dividend (scratch_a).
                    yield_div_rem_64!(
                        Operator::I64RemU,
                        op!(Operator::LocalGet {
                            local_index: abi.scratch_a
                        })
                    );
                }
                StackOp::RemS64 => {
                    yield_div_rem_64!(
                        Operator::I64RemS,
                        op!(Operator::LocalGet {
                            local_index: abi.scratch_a
                        })
                    );
                }
                StackOp::And64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64And,
                    };
                }
                StackOp::Or64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Or,
                    };
                }
                StackOp::Xor64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Xor,
                    };
                }
                StackOp::Shl64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Shl,
                    };
                }
                StackOp::ShrU64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64ShrU,
                    };
                }
                StackOp::ShrS64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64ShrS,
                    };
                }

                // ── Comparisons (i32 result extended to i64) ──────────────
                StackOp::Eq64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Eq,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64ExtendI32U,
                    };
                }
                StackOp::Ne64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64Ne,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64ExtendI32U,
                    };
                }
                StackOp::LtU64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64LtU,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64ExtendI32U,
                    };
                }
                StackOp::LtS64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64LtS,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64ExtendI32U,
                    };
                }
                StackOp::GeU64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64GeU,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64ExtendI32U,
                    };
                }
                StackOp::GeS64 => {
                    yield JitOpcode::Operator {
                        op: Operator::I64GeS,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64ExtendI32U,
                    };
                }

                // ── Conversions ───────────────────────────────────────────
                StackOp::Trunc32 => {
                    yield JitOpcode::Operator {
                        op: Operator::I32WrapI64,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64ExtendI32U,
                    };
                }
                StackOp::SignExt32 => {
                    yield JitOpcode::Operator {
                        op: Operator::I32WrapI64,
                    };
                    yield JitOpcode::Operator {
                        op: Operator::I64ExtendI32S,
                    };
                }
                // No-ops in WASM (i64 is two's-complement throughout).
                StackOp::AsSigned64 | StackOp::AsUnsigned64 => {}

                // ── High-word multiplies (stub — deferred, see fn doc) ─────
                StackOp::MulHU64 | StackOp::MulHSU64 | StackOp::MulHS64 => {
                    op!(Operator::Unreachable);
                }

                // ── 32-bit arithmetic ──────────────────────────────────────
                StackOp::Add32 => yield_binop32!(Operator::I32Add),
                StackOp::Sub32 => yield_binop32!(Operator::I32Sub),
                StackOp::Mul32 => yield_binop32!(Operator::I32Mul),
                StackOp::Shl32 => yield_binop32!(Operator::I32Shl),
                StackOp::ShrU32 => yield_binop32!(Operator::I32ShrU),
                StackOp::ShrS32 => yield_binop32!(Operator::I32ShrS),
                StackOp::DivU32 => {
                    yield_div_rem_32!(Operator::I32DivU, op!(Operator::I64Const { value: -1 }));
                }
                StackOp::DivS32 => {
                    yield_div_rem_32!(Operator::I32DivS, op!(Operator::I64Const { value: -1 }));
                }
                StackOp::RemU32 => {
                    // Remainder-by-zero: the dividend's low 32 bits,
                    // sign-extended back to i64 (matching the Else branch).
                    yield_div_rem_32!(
                        Operator::I32RemU,
                        op!(Operator::LocalGet {
                            local_index: abi.scratch_a
                        })
                    );
                }
                StackOp::RemS32 => {
                    yield_div_rem_32!(
                        Operator::I32RemS,
                        op!(Operator::LocalGet {
                            local_index: abi.scratch_a
                        })
                    );
                }

                // ── Memory ──────────────────────────────────────────────────
                StackOp::LoadMem { width, signed } => {
                    op!(Operator::I32WrapI64); // addr: i64 -> i32 (flat memory model)
                    match (width, signed) {
                        (MemWidth::B1, true) => op!(Operator::I64Load8S {
                            memarg: mem_arg!(0)
                        }),
                        (MemWidth::B1, false) => op!(Operator::I64Load8U {
                            memarg: mem_arg!(0)
                        }),
                        (MemWidth::B2, true) => op!(Operator::I64Load16S {
                            memarg: mem_arg!(1)
                        }),
                        (MemWidth::B2, false) => op!(Operator::I64Load16U {
                            memarg: mem_arg!(1)
                        }),
                        (MemWidth::B4, true) => op!(Operator::I64Load32S {
                            memarg: mem_arg!(2)
                        }),
                        (MemWidth::B4, false) => op!(Operator::I64Load32U {
                            memarg: mem_arg!(2)
                        }),
                        (MemWidth::B8, _) => op!(Operator::I64Load {
                            memarg: mem_arg!(3)
                        }),
                    }
                }
                StackOp::StoreMem { width } => {
                    // Stack: [addr: i64, value: i64] (addr below, value on
                    // top) — matches WASM store's own [addr, value] operand
                    // order, so only `addr` (now buried below `value`) needs
                    // reordering through a scratch local to wrap it to i32.
                    op!(Operator::LocalSet {
                        local_index: abi.scratch_a
                    }); // pop value
                    op!(Operator::I32WrapI64); // wrap addr -> i32
                    op!(Operator::LocalGet {
                        local_index: abi.scratch_a
                    }); // push value back
                    match width {
                        MemWidth::B1 => op!(Operator::I64Store8 {
                            memarg: mem_arg!(0)
                        }),
                        MemWidth::B2 => op!(Operator::I64Store16 {
                            memarg: mem_arg!(1)
                        }),
                        MemWidth::B4 => op!(Operator::I64Store32 {
                            memarg: mem_arg!(2)
                        }),
                        MemWidth::B8 => op!(Operator::I64Store {
                            memarg: mem_arg!(3)
                        }),
                    }
                }

                // ── Side effects ────────────────────────────────────────────
                StackOp::Ecall => {
                    for r in 0..abi.num_regs {
                        op!(Operator::LocalGet {
                            local_index: abi.reg_base + r
                        });
                    }
                    op!(Operator::Call {
                        function_index: abi.ecall_func_idx
                    });
                    for r in (0..abi.num_regs).rev() {
                        op!(Operator::LocalSet {
                            local_index: abi.reg_base + r
                        });
                    }
                }
                StackOp::Trap(_msg) => {
                    op!(Operator::Unreachable);
                }
                StackOp::TailCall => {
                    // Stack: [addr: i64] — save it before pushing registers
                    // (which must precede it as `target_pc`, the last
                    // argument, per the shared calling convention), then
                    // push it back.
                    op!(Operator::LocalSet {
                        local_index: abi.scratch_a
                    });
                    for r in 0..abi.num_regs {
                        op!(Operator::LocalGet {
                            local_index: abi.reg_base + r
                        });
                    }
                    op!(Operator::LocalGet {
                        local_index: abi.scratch_a
                    });
                    op!(Operator::ReturnCall {
                        function_index: abi.lookup_stub_func_idx
                    });
                }
                StackOp::Log(_) => {} // diagnostic hint — no WASM equivalent

                // ── Deferred (see fn doc) ───────────────────────────────────
                StackOp::FloatUnary { .. }
                | StackOp::FloatBinary { .. }
                | StackOp::FloatLt { .. }
                | StackOp::FloatEq { .. }
                | StackOp::FloatUnordered { .. }
                | StackOp::IntToFloat { .. }
                | StackOp::FloatToInt { .. }
                | StackOp::FloatConvert { .. } => {
                    op!(Operator::Unreachable);
                }

                // ── Instruction integrity check ─────────────────────────────
                StackOp::CheckCode {
                    addr,
                    expected,
                    miss_label: _,
                } => {
                    let cmp = match expected.len() {
                        4 => Some((
                            u32::from_le_bytes([expected[0], expected[1], expected[2], expected[3]])
                                as i32,
                            Operator::I32Load {
                                memarg: mem_arg!(2),
                            },
                        )),
                        2 => Some((
                            u16::from_le_bytes([expected[0], expected[1]]) as i32,
                            Operator::I32Load16U {
                                memarg: mem_arg!(1),
                            },
                        )),
                        _ => None, // unknown width — skip check, matching the JS renderer
                    };
                    if let Some((expected_i32, load_op)) = cmp {
                        op!(Operator::I32Const {
                            value: (addr as u32) as i32
                        }); // flat memory model
                        op!(load_op);
                        op!(Operator::I32Const {
                            value: expected_i32
                        });
                        op!(Operator::I32Ne);
                        op!(Operator::If {
                            blockty: wasmparser::BlockType::Empty
                        });
                        op!(Operator::I64Const { value: addr as i64 });
                        op!(Operator::Call {
                            function_index: abi.jit_invalidate_func_idx
                        });
                        for r in 0..abi.num_regs {
                            op!(Operator::LocalGet {
                                local_index: abi.reg_base + r
                            });
                        }
                        op!(Operator::I64Const { value: addr as i64 });
                        op!(Operator::ReturnCall {
                            function_index: abi.lookup_stub_func_idx
                        });
                        op!(Operator::End);
                    }
                }
            }
        }
    }
}
