//! Verifies Phase 1 of the AArch64/function-references plan: named
//! `StackOp::LoadState`/`StoreState` traces (the *only* way AArch64's
//! frontend addresses its GPRs/NZCV/V-registers — it never emits
//! `LoadReg`/`StoreReg`) lower correctly through
//! `vane_arch::template::render_stack_to_wasm` and execute correctly in
//! `wasmi`, using the real `AARCH64_STATE_SLOTS` table and the canonical
//! `reg_base = 0, num_regs = 73` convention documented on
//! `WasmAbiConfig::state_slots`.

use alloc::vec;
use alloc::vec::Vec;

use vane_arch::template::render_stack_to_wasm;
use vane_arch::{JitOpcode, WasmAbiConfig};
use vane_target_core::StackOp;
use wasm_encoder::reencode::{RoundtripReencoder, utils::instruction};
use wasm_encoder::{
    CodeSection, ExportKind, ExportSection, Function, FunctionSection, ImportSection, Instruction,
    MemorySection, MemoryType, Module, TypeSection, ValType,
};

use crate::AARCH64_STATE_SLOTS;

const NUM_REGS: u32 = 73;
const TARGET_PC_LOCAL: u32 = NUM_REGS;
const SCRATCH_A: u32 = NUM_REGS + 1;
const SCRATCH_B: u32 = NUM_REGS + 2;

fn abi() -> WasmAbiConfig {
    WasmAbiConfig {
        reg_base: 0,
        num_regs: NUM_REGS,
        target_pc_local: TARGET_PC_LOCAL,
        scratch_a: SCRATCH_A,
        scratch_b: SCRATCH_B,
        mem_idx: 0,
        ecall_func_idx: 0,
        jit_invalidate_func_idx: 1,
        lookup_stub_func_idx: 2,
        state_slots: &AARCH64_STATE_SLOTS,
    }
}

fn render(ops: Vec<StackOp>) -> Function {
    let mut f = Function::new([(1, ValType::I64), (1, ValType::I64)]); // scratch_a, scratch_b
    let mut reencoder = RoundtripReencoder;
    for jit_op in render_stack_to_wasm(ops.into_iter(), abi()) {
        let JitOpcode::Operator { op } = jit_op;
        let instr = instruction(&mut reencoder, op).expect("reencode should not fail");
        f.instruction(&instr);
    }
    f.instruction(&Instruction::End);
    f
}

fn regfn_params() -> Vec<ValType> {
    vec![ValType::I64; (TARGET_PC_LOCAL + 1) as usize]
}
fn regfn_results() -> Vec<ValType> {
    vec![ValType::I64; NUM_REGS as usize]
}

/// Reads back every one of the 73 registers via plain indexed `LoadReg`,
/// deliberately *not* `LoadState` — this is what proves a named
/// `StoreState` and an indexed `LoadReg` at the same index alias the exact
/// same local, i.e. that `state_slots` entries really do live within
/// `reg_base .. reg_base + num_regs` rather than in separate locals.
fn tail_via_load_reg() -> Vec<StackOp> {
    (0..NUM_REGS).map(|i| StackOp::LoadReg(i as u8)).collect()
}

fn build_and_instantiate(functions: Vec<(&str, Function)>) -> (wasmi::Store<()>, wasmi::Instance) {
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
        .section(&code);
    let wasm = module.finish();

    let mut config = wasmi::Config::default();
    config.wasm_tail_call(true);
    let engine = wasmi::Engine::new(&config);
    let mut store = wasmi::Store::new(&engine, ());
    let mut linker: wasmi::Linker<()> = wasmi::Linker::new(&engine);
    let ecall_ty = wasmi::FuncType::new(
        vec![wasmi::ValType::I64; NUM_REGS as usize],
        vec![wasmi::ValType::I64; NUM_REGS as usize],
    );
    linker
        .func_new("env", "ecall", ecall_ty, |_caller: wasmi::Caller<'_, ()>, params: &[wasmi::Val], results: &mut [wasmi::Val]| {
            results.clone_from_slice(params);
            Ok(())
        })
        .unwrap();
    linker.func_wrap("env", "jit_invalidate", |_caller: wasmi::Caller<'_, ()>, _pc: i64| {}).unwrap();
    let wasmi_module = wasmi::Module::new(&engine, &wasm[..]).expect("module should validate");
    let instance = linker
        .instantiate_and_start(&mut store, &wasmi_module)
        .expect("module should instantiate");
    (store, instance)
}

fn call(store: &mut wasmi::Store<()>, instance: &wasmi::Instance, name: &str) -> Result<Vec<i64>, wasmi::Error> {
    let func = instance.get_func(&mut *store, name).expect("export should exist");
    let params = vec![wasmi::Val::I64(0); (TARGET_PC_LOCAL + 1) as usize];
    let mut results = vec![wasmi::Val::I64(0); NUM_REGS as usize];
    func.call(&mut *store, &params, &mut results)?;
    Ok(results
        .into_iter()
        .map(|v| match v {
            wasmi::Val::I64(x) => x,
            other => panic!("unexpected result type: {other:?}"),
        })
        .collect())
}

fn index_of(name: &str) -> u32 {
    AARCH64_STATE_SLOTS
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no such state slot: {name}"))
        .1
}

#[test]
fn store_state_gpr_aliases_the_same_local_as_indexed_load_reg() {
    let idx = index_of("a64:x5");
    let mut ops = vec![StackOp::PushImm(0x1234), StackOp::StoreState(crate::state::x(5))];
    ops.extend(tail_via_load_reg());
    let f = render(ops);
    let (mut store, instance) = build_and_instantiate(vec![("f", f)]);
    let results = call(&mut store, &instance, "f").expect("call should not trap");
    for (i, r) in results.iter().enumerate() {
        if i as u32 == idx {
            assert_eq!(*r, 0x1234, "StoreState(a64:x5) must land in the same local LoadReg(5) reads");
        } else {
            assert_eq!(*r, 0, "no other local should be touched");
        }
    }
}

#[test]
fn nzcv_flag_round_trips_through_load_state_and_store_state() {
    let idx = index_of("a64:nzcv:z");
    let mut ops = vec![
        StackOp::PushImm(1),
        StackOp::StoreState(crate::state::nzcv('z')),
        StackOp::LoadState(crate::state::nzcv('z')),
        StackOp::StoreReg(idx as u8),
    ];
    ops.extend(tail_via_load_reg());
    let f = render(ops);
    let (mut store, instance) = build_and_instantiate(vec![("f", f)]);
    let results = call(&mut store, &instance, "f").expect("call should not trap");
    assert_eq!(results[idx as usize], 1, "nzcv:z must round-trip through StoreState/LoadState");
}

#[test]
fn v_register_round_trips_through_load_state_and_store_state() {
    let idx = index_of("a64:v10");
    let mut ops = vec![
        StackOp::PushImm(0xdead_beef),
        StackOp::StoreState(crate::state::v(10)),
        StackOp::LoadState(crate::state::v(10)),
        StackOp::StoreReg(idx as u8),
    ];
    ops.extend(tail_via_load_reg());
    let f = render(ops);
    let (mut store, instance) = build_and_instantiate(vec![("f", f)]);
    let results = call(&mut store, &instance, "f").expect("call should not trap");
    assert_eq!(results[idx as usize], 0xdead_beef);
}

#[test]
fn unmapped_state_slot_name_traps_on_unreachable() {
    let mut ops = vec![StackOp::LoadState(vane_target_core::StateSlot::new("a64:not-a-real-slot"))];
    ops.extend(tail_via_load_reg());
    let f = render(ops);
    let (mut store, instance) = build_and_instantiate(vec![("f", f)]);
    let result = call(&mut store, &instance, "f");
    assert!(result.is_err(), "an unmapped state-slot name must still trap, not silently zero");
}
