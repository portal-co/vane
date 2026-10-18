//! RV64 glue: building Vane's RV64 trace for one instruction word and the
//! Unicorn RISCV64 profile.
//!
//! `emit_rv64` reproduces exactly what `Aarch64Reactor::run` /
//! `riscv_fp_tests` do: a `TemplateJit` over the harness's `Mem` with
//! `Heat::New` (never cache-hit) and default flags, feeding `rv_emit`.

use alloc::vec::Vec;
use vane_arch::{Heat, Mem};
use vane_arch::template::{Params, TemplateJit};
use vane_target_core::StackOp;

use crate::runner::CrossRunner;

/// Build the RV64 `StackOp` trace for the instruction word at `pc` from the
/// runner's `Mem`. `trial` is pinned to `Heat::New` so the trace always
/// covers the instruction body rather than short-circuiting to a cached
/// `TailCall`.
///
/// **Scope note:** `rv_emit` is a basic-block tracer — it recursively
/// inlines fallthrough and branch successors until a `TailCall`/`Trap`. The
/// differential loop therefore bounds a trace's scope by placing a
/// dedicated trap-pad word (`STOP_PAD`, all-ones) after every generated
/// instruction and treating the pad's `Exit::Trap` as "instruction under
/// test completed, control reached the pad PC". This keeps the compared
/// scope to exactly one instruction without reimplementing `rv_body`'s
/// flow logic, and it composes with taken branches/jumps (they tail-call
/// out of the trace instead of reaching the pad).
pub fn emit_rv64(mem: &mut Mem, pc: u64) -> Vec<StackOp> {
    let jit = TemplateJit {
        params: Params {
            react: mem,
            trial: &|_| Heat::New,
            root: 0,
            flags: vane_arch::template::Flags::default(),
            boundaries: None,
        },
        pc,
        labels: Default::default(),
        depth: 0,
    };
    vane_riscv::fuzz::rv_emit(&jit)
}

/// Alias so tests/fuzz targets can name the runner type directly.
pub type Rv64Runner = CrossRunner<Rv64Profile>;

/// RV64 profile: Unicorn `Arch::RISCV` in `Mode::RISCV64`, Vane x0..x31
/// with x0 hardwired to zero.
pub struct Rv64Profile;

impl crate::runner::ArchProfile for Rv64Profile {

    fn unicorn() -> (unicorn_engine::unicorn_const::Arch, unicorn_engine::unicorn_const::Mode) {
        (
            unicorn_engine::unicorn_const::Arch::RISCV,
            unicorn_engine::unicorn_const::Mode::RISCV64,
        )
    }

    fn unicorn_write_regs(uc: &mut unicorn_engine::Unicorn<()>, regs: &[u64; 32]) {
        for (i, v) in regs.iter().enumerate() {
            uc.reg_write(RegisterRISCV::X0 as i32 + i as i32, *v)
                .expect("unicorn reg_write x");
        }
    }

    fn unicorn_read_regs(uc: &mut unicorn_engine::Unicorn<()>) -> [u64; 32] {
        core::array::from_fn(|i| {
            uc.reg_read(RegisterRISCV::X0 as i32 + i as i32)
                .expect("unicorn reg_read x")
        })
    }

    fn unicorn_read_pc(uc: &mut unicorn_engine::Unicorn<()>) -> u64 {
        uc.reg_read(RegisterRISCV::PC).expect("unicorn reg_read pc")
    }

    fn reg_names() -> [&'static str; 32] {
        core::array::from_fn(|i| {
            if i == 0 {
                "x0"
            } else {
                "x"
            }
        })
    }

    fn pad_trap_msg() -> &'static str {
        "op:ebreak"
    }

    fn pad_word() -> u32 {
        crate::runner::STOP_PAD
    }

    fn reg_zero_is_hardwired() -> bool {
        true
    }
}

use unicorn_engine::RegisterRISCV;

/// Execute one differential step for RV64: build Vane's trace for `pc` from
/// the runner's memory and compare against Unicorn's single step.
pub fn step_rv64(runner: &mut Rv64Runner, pc: u64) -> crate::runner::Step {
    runner.step(pc, |mem| emit_rv64(mem, pc))
}
