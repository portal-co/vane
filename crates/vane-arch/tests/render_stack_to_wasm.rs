//! Integration tests for `vane_arch::template::render_stack_to_wasm` (Phase 0
//! of the dynamic-JIT plan): register convention, memory access, ecall,
//! tail-call, and instruction-integrity-check lowering, executed end-to-end
//! in `wasmi`.

use vane_arch::template::render_stack_to_wasm;
use vane_arch::{JitOpcode, WasmAbiConfig};
use vane_target_core::{MemWidth, StackOp};
use wasm_encoder::reencode::{RoundtripReencoder, utils::instruction};
use wasm_encoder::{
    CodeSection, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction, MemorySection, MemoryType, Module, TypeSection, ValType,
};

const NUM_REGS: u32 = 3;
// Locals layout: r0,r1,r2 (params 0..2), target_pc (param 3), scratch_a (4),
// scratch_b (5) — matching `WasmAbiConfig`.
const TARGET_PC_LOCAL: u32 = NUM_REGS;
const SCRATCH_A: u32 = NUM_REGS + 1;
const SCRATCH_B: u32 = NUM_REGS + 2;

fn abi(mem_idx: u32, ecall_func_idx: u32, jit_invalidate_func_idx: u32, lookup_stub_func_idx: u32) -> WasmAbiConfig {
    WasmAbiConfig {
        reg_base: 0,
        num_regs: NUM_REGS,
        target_pc_local: TARGET_PC_LOCAL,
        scratch_a: SCRATCH_A,
        scratch_b: SCRATCH_B,
        mem_idx,
        ecall_func_idx,
        jit_invalidate_func_idx,
        lookup_stub_func_idx,
        state_slots: &[],
    }
}

/// Render `ops` into a `wasm_encoder::Function` of type `regfn` (see
/// `regfn_params`/`regfn_results`): params are exactly `r0,r1,r2,target_pc`
/// (matching speet's `OobConfig` shared calling convention, so this is
/// directly `return_call`-compatible with any other `regfn`-typed function);
/// `scratch_a`/`scratch_b` are genuine private locals, declared here, not
/// externally visible params.
fn render(ops: Vec<StackOp>, config: WasmAbiConfig) -> Function {
    let mut f = Function::new([(1, ValType::I64), (1, ValType::I64)]); // scratch_a, scratch_b
    let mut reencoder = RoundtripReencoder;
    for jit_op in render_stack_to_wasm(ops.into_iter(), config) {
        let JitOpcode::Operator { op } = jit_op;
        let instr = instruction(&mut reencoder, op).expect("reencode should not fail");
        f.instruction(&instr);
    }
    f.instruction(&Instruction::End);
    f
}

/// regfn type: (r0,r1,r2,target_pc : i64) -> (r0,r1,r2 : i64) — matching
/// `OobConfig`'s shared calling convention exactly (arch regs + target_pc ->
/// arch regs), used uniformly for every function in these tests (including
/// hand-built `TailCall`/`CheckCode` targets) so `return_call` type-checks.
fn regfn_params() -> Vec<ValType> {
    vec![ValType::I64; (TARGET_PC_LOCAL + 1) as usize]
}
fn regfn_results() -> Vec<ValType> {
    vec![ValType::I64; NUM_REGS as usize]
}

struct HostState {
    invalidated: Vec<i64>,
}

fn build_and_instantiate(
    functions: Vec<(&str, Function)>,
    mem_bytes: &[(u32, Vec<u8>)],
) -> (wasmi::Store<HostState>, wasmi::Instance) {
    let mut types = TypeSection::new();
    types.ty().function(regfn_params(), regfn_results()); // type 0: regfn
    types.ty().function([ValType::I64; NUM_REGS as usize], [ValType::I64; NUM_REGS as usize]); // type 1: ecall
    types.ty().function([ValType::I64], []); // type 2: jit_invalidate

    let mut imports = ImportSection::new();
    imports.import("env", "ecall", wasm_encoder::EntityType::Function(1)); // func 0
    imports.import("env", "jit_invalidate", wasm_encoder::EntityType::Function(2)); // func 1

    let mut func_section = FunctionSection::new();
    for _ in &functions {
        func_section.function(0);
    }

    let mut memories = MemorySection::new();
    memories.memory(MemoryType { minimum: 1, maximum: None, memory64: false, shared: false, page_size_log2: None });

    let mut exports = ExportSection::new();
    for (i, (name, _)) in functions.iter().enumerate() {
        exports.export(name, ExportKind::Func, 2 + i as u32); // +2 for the two imports
    }
    exports.export("memory", ExportKind::Memory, 0);

    let mut data = wasm_encoder::DataSection::new();
    for (offset, bytes) in mem_bytes {
        data.active(0, &wasm_encoder::ConstExpr::i32_const(*offset as i32), bytes.clone());
    }

    let mut code = CodeSection::new();
    for (_, f) in &functions {
        code.function(f);
    }

    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&func_section)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&data);
    let wasm = module.finish();

    let mut config = wasmi::Config::default();
    config.wasm_tail_call(true);
    let engine = wasmi::Engine::new(&config);
    let mut store = wasmi::Store::new(&engine, HostState { invalidated: Vec::new() });
    let mut linker: wasmi::Linker<HostState> = wasmi::Linker::new(&engine);
    linker
        .func_wrap(
            "env",
            "ecall",
            // Adds to r1, not r0: r0 is register 0 (hardwired zero per
            // RISC-V/StackOp semantics — see `register_zero_is_always_zero`),
            // so modifying it wouldn't be observable through `LoadReg(0)`.
            |_caller: wasmi::Caller<'_, HostState>, r0: i64, r1: i64, r2: i64| (r0, r1 + 1000, r2),
        )
        .unwrap();
    linker
        .func_wrap("env", "jit_invalidate", |mut caller: wasmi::Caller<'_, HostState>, pc: i64| {
            caller.data_mut().invalidated.push(pc);
        })
        .unwrap();
    let wasmi_module = wasmi::Module::new(&engine, &wasm[..]).expect("module should validate");
    let instance = linker
        .instantiate_and_start(&mut store, &wasmi_module)
        .expect("module should instantiate");
    (store, instance)
}

fn call(
    store: &mut wasmi::Store<HostState>,
    instance: &wasmi::Instance,
    name: &str,
    regs: (i64, i64, i64),
    target_pc: i64,
) -> (i64, i64, i64) {
    let func = instance.get_func(&mut *store, name).expect("export should exist");
    let params = [
        wasmi::Val::I64(regs.0),
        wasmi::Val::I64(regs.1),
        wasmi::Val::I64(regs.2),
        wasmi::Val::I64(target_pc),
    ];
    let mut results = vec![wasmi::Val::I64(0); NUM_REGS as usize];
    func.call(&mut *store, &params, &mut results).expect("call should not trap");
    let get = |v: &wasmi::Val| match v {
        wasmi::Val::I64(x) => *x,
        other => panic!("unexpected result type: {other:?}"),
    };
    (get(&results[0]), get(&results[1]), get(&results[2]))
}

/// Common ending for hand-written `StackOp` bodies that don't otherwise
/// diverge: leave (r0, r1, r2) as the function's three results.
fn tail() -> Vec<StackOp> {
    vec![StackOp::LoadReg(0), StackOp::LoadReg(1), StackOp::LoadReg(2)]
}

#[test]
fn register_zero_is_always_zero_and_store_to_it_is_dropped() {
    let mut ops = vec![
        StackOp::PushImm(99),
        StackOp::StoreReg(0), // must be dropped, not written anywhere observable
        StackOp::LoadReg(0),  // must push 0 regardless of local 0's actual value
        StackOp::StoreReg(1),
    ];
    ops.extend(tail());
    let f = render(ops, abi(0, 0, 1, 2));
    let (mut store, instance) = build_and_instantiate(vec![("f", f)], &[]);
    // Pass a nonzero value in the r0 param slot; LoadReg(0) must still push 0
    // for the r0 result, and StoreReg(0) must not have corrupted anything
    // observable (r1's result, driven entirely by the earlier StoreReg(1)).
    let (_r0, r1, _r2) = call(&mut store, &instance, "f", (777, 0, 0), 0);
    assert_eq!(r1, 0, "LoadReg(0) must push 0 unconditionally, independent of r0's actual param value");
}

#[test]
fn div32_and_rem32_by_zero_both_yield_all_ones() {
    // Matches this codebase's established, tested convention (the JS
    // renderer and the bytecode interpreter — see
    // `interp::tests::div_and_rem_by_zero_match_js_renderer` — both yield
    // all-ones for div *and* rem on a zero divisor), not raw RISC-V ISA
    // semantics (which defines remainder-by-zero as the dividend).
    let mut div_ops = vec![StackOp::PushImm(42), StackOp::PushImm(0), StackOp::DivU32, StackOp::StoreReg(1)];
    div_ops.extend(tail());
    let mut rem_ops = vec![StackOp::PushImm(42), StackOp::PushImm(0), StackOp::RemU32, StackOp::StoreReg(1)];
    rem_ops.extend(tail());
    let div_f = render(div_ops, abi(0, 0, 1, 2));
    let rem_f = render(rem_ops, abi(0, 0, 1, 2));
    let (mut store, instance) = build_and_instantiate(vec![("div", div_f), ("rem", rem_f)], &[]);

    let (_, r1, _) = call(&mut store, &instance, "div", (0, 0, 0), 0);
    assert_eq!(r1, -1, "DivU32 by zero must yield all-ones, not trap");

    let (_, r1, _) = call(&mut store, &instance, "rem", (0, 0, 0), 0);
    assert_eq!(r1, -1, "RemU32 by zero must also yield all-ones, matching this codebase's convention");
}

#[test]
fn div32_normal_case_matches_expected_quotient() {
    let mut ops = vec![StackOp::PushImm(10), StackOp::PushImm(3), StackOp::DivU32, StackOp::StoreReg(1)];
    ops.extend(tail());
    let f = render(ops, abi(0, 0, 1, 2));
    let (mut store, instance) = build_and_instantiate(vec![("f", f)], &[]);
    let (_, r1, _) = call(&mut store, &instance, "f", (0, 0, 0), 0);
    assert_eq!(r1, 3);
}

#[test]
fn load_and_store_mem_round_trip_flat_addressing() {
    let mut ops = vec![
        StackOp::PushImm(0x100),   // addr
        StackOp::PushImm(0xdead_beef),
        StackOp::StoreMem { width: MemWidth::B8 },
        StackOp::PushImm(0x100),   // addr
        StackOp::LoadMem { width: MemWidth::B8, signed: false },
        StackOp::StoreReg(1),
    ];
    ops.extend(tail());
    let f = render(ops, abi(0, 0, 1, 2));
    let (mut store, instance) = build_and_instantiate(vec![("f", f)], &[]);
    let (_, r1, _) = call(&mut store, &instance, "f", (0, 0, 0), 0);
    assert_eq!(r1, 0xdead_beef);
}

#[test]
fn load_mem_signed_byte_sign_extends() {
    let mut ops = vec![
        StackOp::PushImm(0x200),
        StackOp::PushImm(0xff), // low byte 0xff
        StackOp::StoreMem { width: MemWidth::B1 },
        StackOp::PushImm(0x200),
        StackOp::LoadMem { width: MemWidth::B1, signed: true },
        StackOp::StoreReg(1),
    ];
    ops.extend(tail());
    let f = render(ops, abi(0, 0, 1, 2));
    let (mut store, instance) = build_and_instantiate(vec![("f", f)], &[]);
    let (_, r1, _) = call(&mut store, &instance, "f", (0, 0, 0), 0);
    assert_eq!(r1, -1, "0xff as a signed byte sign-extends to -1");
}

#[test]
fn ecall_threads_all_registers_through_the_host_import() {
    let mut ops = vec![StackOp::Ecall];
    ops.extend(tail());
    let f = render(ops, abi(0, 0, 1, 2));
    let (mut store, instance) = build_and_instantiate(vec![("f", f)], &[]);
    // r0 param is 5 but LoadReg(0)/register-0-is-hardwired-zero means the r0
    // *result* is always 0 regardless — checked via the other test.
    let (_r0, r1, r2) = call(&mut store, &instance, "f", (5, 6, 7), 0);
    assert_eq!((r1, r2), (1006, 7), "host ecall adds 1000 to r1, passes r2 through");
}

#[test]
fn tail_call_reaches_target_with_registers_preserved() {
    // "echo" plays the role of the OOB lookup stub: returns target_pc as the
    // new r0, keeping r1/r2 from its own params (which the tail-caller must
    // have forwarded unchanged).
    let mut echo = Function::new([]);
    echo.instruction(&Instruction::LocalGet(TARGET_PC_LOCAL));
    echo.instruction(&Instruction::LocalGet(1));
    echo.instruction(&Instruction::LocalGet(2));
    echo.instruction(&Instruction::End);

    // lookup_stub_func_idx below must be echo's absolute func index: 2
    // imports + 1 (this function itself is index 0 among defined funcs, so
    // echo, added second, is defined-func-index 1 => absolute index 2+1=3).
    // The tail-call address (0xABC) is a fresh constant, deliberately
    // distinct from any register's value, so a mixed-up forwarding order
    // would show up as a value mismatch rather than accidentally matching.
    let caller_ops = vec![StackOp::PushImm(0xABC), StackOp::TailCall];
    let caller = render(caller_ops, abi(0, 0, 1, /* lookup_stub_func_idx */ 3));

    let (mut store, instance) = build_and_instantiate(vec![("caller", caller), ("echo", echo)], &[]);
    let (r0, r1, r2) = call(&mut store, &instance, "caller", (11, 777, 33), 0);
    assert_eq!((r0, r1, r2), (0xABC, 777, 33), "tail call must land on echo with 0xABC as target_pc and r1/r2 forwarded unchanged, then echo returns (target_pc, r1, r2)");
}

#[test]
fn check_code_match_falls_through_without_invalidating() {
    let addr = 0x300u64;
    let expected: Box<[u8]> = Box::new(0x1234_5678u32.to_le_bytes());
    let mut ops = vec![
        StackOp::CheckCode { addr, expected, miss_label: "m".into() },
        StackOp::PushImm(42),
        StackOp::StoreReg(1),
    ];
    ops.extend(tail());
    let f = render(ops, abi(0, 0, 1, 2));
    let mem = (addr as u32, 0x1234_5678u32.to_le_bytes().to_vec());
    let (mut store, instance) = build_and_instantiate(vec![("f", f)], &[mem]);
    let (_, r1, _) = call(&mut store, &instance, "f", (0, 0, 0), 0);
    assert_eq!(r1, 42, "matching code must fall through to the rest of the function");
    assert!(store.data().invalidated.is_empty());
}

#[test]
fn check_code_mismatch_invalidates_and_tail_calls_lookup_stub() {
    let mut echo = Function::new([]);
    echo.instruction(&Instruction::LocalGet(TARGET_PC_LOCAL));
    echo.instruction(&Instruction::LocalGet(1));
    echo.instruction(&Instruction::LocalGet(2));
    echo.instruction(&Instruction::End);

    let addr = 0x300u64;
    let expected: Box<[u8]> = Box::new(0x1234_5678u32.to_le_bytes());
    let mut ops = vec![
        StackOp::CheckCode { addr, expected, miss_label: "m".into() },
        StackOp::PushImm(999), // unreached if mismatch tail-calls away first
        StackOp::StoreReg(1),
    ];
    ops.extend(tail()); // needed for the (unexercised-here) match-fallthrough path to validate
    let f = render(ops, abi(0, 0, 1, /* lookup_stub_func_idx */ 3));
    // Actual memory content deliberately differs from `expected`.
    let mem = (addr as u32, 0xffff_ffffu32.to_le_bytes().to_vec());
    let (mut store, instance) = build_and_instantiate(vec![("f", f), ("echo", echo)], &[mem]);
    let (r0, _, _) = call(&mut store, &instance, "f", (0, 0, 0), 0);
    assert_eq!(r0, addr as i64, "mismatch must tail-call the lookup stub with addr as target_pc");
    assert_eq!(store.data().invalidated, vec![addr as i64], "mismatch must invalidate the cache entry for addr");
}
