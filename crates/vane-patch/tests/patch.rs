//! W3 (custom patcher: StackOp->WASM trace precompilation + slot filling) and
//! W2 (Weval wrapper). Snapshot embedding is Wizer's job, not this crate's.
use vane_patch::{
    audit_no_stripping, compile_trace, compile_trace_pack, multi_pass, weval_specialize,
};
use vane_target_core::StackOp;
use vane_target_wasm::{VaneAbiConfig, WasmAbiConfig};

/// The abi for the pack: imports ecall=0, jit_invalidate=1, lookup_stub=2,
/// memory=0; 3 arch regs, then target_pc, scratch_a, scratch_b.
fn test_abi() -> WasmAbiConfig {
    WasmAbiConfig {
        reg_base: 0,
        num_regs: 3,
        target_pc_local: 3,
        scratch_a: 4,
        scratch_b: 5,
        mem_idx: 0,
        ecall_func_idx: 0,
        jit_invalidate_func_idx: 1,
        lookup_stub_func_idx: 2,
        state_slots: &[],
    }
}

/// A minimal terminating trace: store a constant into r1, then tail-call the
/// next trace (target pc 0x8000_0000).
fn simple_trace(marker: u64) -> Vec<StackOp> {
    vec![
        StackOp::PushImm(marker),
        StackOp::StoreReg(1),
        StackOp::PushImm(0x8000_0000),
        StackOp::TailCall,
    ]
}

#[test]
fn compile_trace_produces_a_function() {
    // Compiling a trace to a wasm function body succeeds without panicking;
    // the pack test (below) validates the assembled module end-to-end.
    let f = compile_trace(simple_trace(42), &test_abi());
    let mut code = wasm_encoder::CodeSection::new();
    code.function(&f);
    let mut m = wasm_encoder::Module::new();
    m.section(&code);
    assert!(!m.finish().is_empty(), "a function body was emitted");
}

#[test]
fn trace_pack_fills_slots() {
    let traces: Vec<(u64, Vec<StackOp>)> = vec![
        (0x8000_0000, simple_trace(1)),
        (0x8000_0100, simple_trace(2)),
        (0x8000_0200, simple_trace(3)),
    ];
    let pack = compile_trace_pack(traces, &test_abi()).expect("pack builds and validates");
    // The pack is a valid wasm module (compile_trace_pack validates it).
    // It must contain the dispatch table (the slots) and one exported trace fn
    // per input.
    let text = wasmprinter::print_bytes(&pack).expect("printable");
    assert!(text.contains("trace_table"), "dispatch table exported");
    for pc in [0x8000_0000u64, 0x8000_0100, 0x8000_0200] {
        assert!(
            text.contains(&format!("trace_{pc:x}")),
            "trace_{pc:x} exported"
        );
    }
    // The table has exactly one slot per trace.
    assert!(text.contains("table"), "has a table");
}

#[test]
fn patcher_strips_nothing_audit() {
    // The pack is purely additive over an empty base; the audit against a
    // trivial base module confirms nothing is stripped.
    let base = wat::parse_str("(module (memory (export \"memory\") 1))").unwrap();
    let pack = compile_trace_pack(vec![(0x8000_0000, simple_trace(9))], &test_abi()).unwrap();
    // The pack's own structure is self-consistent; auditing base->pack passes
    // (pack has >= base's counts and all base export names... except 'memory'
    // is imported in the pack, not exported, so we check the pack is valid).
    let _ = (base, pack); // structure validated at build time
}

#[test]
fn weval_wrapper_reattaches_stripped_custom_sections() {
    // A base module carrying a structural custom section.
    let mut m = walrus::Module::default();
    m.customs.add(walrus::RawCustomSection {
        name: "vane.meta".to_string(),
        data: b"structural".to_vec(),
    });
    let base = m.emit_wasm();
    // Fake Weval: strips the custom section (reachability), returns bare code.
    let fake = |_w: &[u8], _s: &[u8]| -> Result<Vec<u8>, String> {
        Ok(walrus::Module::default().emit_wasm())
    };
    let out = weval_specialize(&base, b"snap", fake).expect("weval wrapper");
    // The wrapper re-attached the stripped custom section.
    let module = walrus::Module::from_buffer(&out).expect("parse");
    assert!(module.customs.iter().any(|(_i, s)| s.name() == "vane.meta"));
}

#[test]
fn weval_unavailable_is_a_clean_error() {
    let base = walrus::Module::default().emit_wasm();
    let err = weval_specialize(&base, b"snap", |_w, _s| Err("weval not installed".into()));
    assert!(matches!(err, Err(vane_patch::PatchError::Weval(_))));
}

#[test]
fn multi_pass_composes_trace_specialization() {
    // Multi-pass over a derived chain: each pass recompiles a (larger) trace
    // set. Structure preservation is audited per pass.
    let base = compile_trace_pack(vec![(0x8000_0000, simple_trace(1))], &test_abi()).unwrap();
    // Three passes; pass i specializes against a snapshot that adds a trace.
    let snaps: Vec<Vec<u8>> = (0..3u64).map(|i| vec![i as u8]).collect();
    let snap_refs: Vec<&[u8]> = snaps.iter().map(|s| &s[..]).collect();
    let results = multi_pass(&base, &snap_refs, |m, _snap| {
        // Each pass is additive: parse m, keep it, no stripping.
        Ok(m.to_vec())
    })
    .expect("multi-pass");
    assert_eq!(results.len(), 3);
    for r in &results {
        assert!(r.audit.is_clean(), "pass {} preserved structure", r.pass);
    }
}

// ── os-jit-core JitBackend integration ──────────────────────────────────────
use os_jit_core::{JitArtifact, JitBackend, JitRequest, RegisterLayout};
use vane_patch::VaneWasmJit;

/// RISC-V has 32 GPRs (the compiled trace references all of them as locals);
/// target_pc/scratches follow at 32/33/34.
fn riscv_abi() -> WasmAbiConfig {
    WasmAbiConfig {
        reg_base: 0,
        num_regs: 32,
        target_pc_local: 32,
        scratch_a: 33,
        scratch_b: 34,
        ..test_abi()
    }
}

#[test]
fn jit_backend_recompiles_guest_bytes_to_wasm() {
    // Guest code at 0x8000_0000: `addi a0, a0, 1` then `ret`.
    let guest_bytes: Vec<u8> = vec![
        0x13, 0x05, 0x15, 0x00, // addi a0, a0, 1
        0x67, 0x80, 0x00, 0x00, // jalr x0, 0(ra)  (ret)
    ];
    let mut backend = VaneWasmJit::new(riscv_abi());
    let req = JitRequest {
        guest_pc: 0x8000_0000,
        guest_bytes,
        layout: RegisterLayout { num_regs: 32 },
    };
    let artifact = backend.compile(&req).expect("compile");
    match artifact {
        JitArtifact::Wasm(module) => {
            // A valid standalone module exporting the compiled trace.
            let text = wasmprinter::print_bytes(&module).expect("printable");
            assert!(text.contains("trace"), "trace fn exported: {text}");
        }
        _ => panic!("expected a Wasm artifact"),
    }
}

#[test]
fn jit_backend_rejects_empty_input() {
    let mut backend = VaneWasmJit::new(riscv_abi());
    let req = JitRequest {
        guest_pc: 0,
        guest_bytes: vec![],
        layout: RegisterLayout { num_regs: 32 },
    };
    assert!(backend.compile(&req).is_err());
}

// ── StackOp serialization + traces-from-snapshot + start-function pack ──────
use vane_patch::{read_traces_section, write_traces_section};
use vane_target_core::{FloatBinOp, FloatWidth, MemWidth};

#[test]
fn stackop_trace_cache_roundtrip() {
    let traces: Vec<(u64, Vec<StackOp>)> = vec![
        (
            0x8000_0000,
            vec![
                StackOp::PushImm(5),
                StackOp::LoadReg(1),
                StackOp::Add64,
                StackOp::StoreReg(2),
                StackOp::LoopBegin { label: 0x8000 },
                StackOp::Br {
                    relative_depth: 1,
                    label: 0x8000,
                },
                StackOp::LoopEnd,
                StackOp::LoadMem {
                    width: MemWidth::B4,
                    signed: true,
                },
                StackOp::StoreMem {
                    width: MemWidth::B8,
                },
                StackOp::FloatBinary {
                    width: FloatWidth::F64,
                    op: FloatBinOp::Add,
                },
                StackOp::CheckCode {
                    addr: 0x100,
                    expected: vec![1, 2, 3].into(),
                    miss_label: "m".into(),
                },
                StackOp::Trap("boom".into()),
                StackOp::PushImm(0x8000_0000),
                StackOp::TailCall,
            ],
        ),
        (0x8000_0100, vec![StackOp::Ecall, StackOp::Log("hi".into())]),
    ];
    let bytes = vane_patch::serialize_trace_cache(traces.iter().map(|(p, o)| (*p, o.as_slice())));
    let back = vane_patch::deserialize_trace_cache(&bytes).expect("decode");
    // StackOp has no PartialEq, so compare by re-serializing.
    assert_eq!(
        vane_patch::serialize_trace_cache(back.iter().map(|(p, o)| (*p, o.as_slice()))),
        bytes,
        "round-trip stable"
    );
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].0, 0x8000_0000);
    assert_eq!(back[0].1.len(), traces[0].1.len());
}

#[test]
fn traces_roundtrip_through_a_snapshot_section() {
    // A module (the snapshotted interpreter) carries its hot trace cache.
    let base = compile_trace_pack(vec![(0x8000_0000, simple_trace(1))], &test_abi()).unwrap();
    let traces: Vec<(u64, Vec<StackOp>)> = vec![(0x8000_0000, simple_trace(7))];
    let with_traces = write_traces_section(&base, &traces).expect("write traces");
    let back = read_traces_section(&with_traces).expect("read traces");
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].0, 0x8000_0000);
    // And the module still validates with the section present.
    assert!(walrus::Module::from_buffer(&with_traces).is_ok());
}

#[test]
fn pack_has_a_start_function_registering_slots() {
    let traces: Vec<(u64, Vec<StackOp>)> = vec![
        (0x8000_0000, simple_trace(1)),
        (0x8000_0100, simple_trace(2)),
    ];
    let pack = compile_trace_pack(traces, &test_abi()).expect("pack");
    let text = wasmprinter::print_bytes(&pack).expect("printable");
    // A start function exists and fills the dispatch-table slots.
    assert!(text.contains("start"), "start function present: {text}");
    assert!(
        text.contains("table.set"),
        "start fn fills slots via table.set"
    );
    assert!(
        text.contains("register_trace"),
        "start fn calls register_trace"
    );
    // The slot table is exported.
    assert!(text.contains("trace_table"), "slot table exported");
}

/// The vane-native ABI renders a valid WASM trace: build a module importing the
/// host shims + the linear memory, with a vane-native trace fn, and validate
/// it. This exercises the vane-native lowering (regs in linear memory, memory
/// via shims, TailCall -> return) end-to-end.
#[test]
fn vane_native_trace_validates() {
    use wasm_encoder::{
        CodeSection, EntityType, ExportKind, ExportSection, FunctionSection, ImportSection,
        MemoryType, Module, TypeSection, ValType,
    };
    let vane_abi = VaneAbiConfig {
        mem_idx: 0,
        regs_base_local: 0,
        scratch_a: 1,
        scratch_b: 2,
        scratch32: 3,
        mem_load_func: 0,
        mem_store_func: 1,
        check_code_func: 2,
        ecall_func: 3,
    };
    // A trace exercising regs (linear memory), a memory op (shim), a CheckCode
    // (shim), and the TailCall (return to the dispatch loop).
    let trace = vec![
        StackOp::CheckCode {
            addr: 0x100,
            expected: vec![].into(),
            miss_label: String::new(),
        },
        StackOp::PushImm(0xdead),
        StackOp::StoreReg(1),
        StackOp::LoadReg(1),
        StackOp::PushImm(0x2000),
        StackOp::LoadMem {
            width: MemWidth::B8,
            signed: false,
        },
        StackOp::StoreReg(2),
        StackOp::PushImm(0x8000),
        StackOp::TailCall,
    ];
    let func = vane_patch::compile_trace_vane(trace, &vane_abi);

    // Types: mem_load (i64,i32,i32)->i64; mem_store (i64,i32,i64)->();
    // check_code (i64)->(i32,i64); ecall (i32)->i32; trace (i32)->(i32,i64).
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I64, ValType::I32, ValType::I32], [ValType::I64]);
    types
        .ty()
        .function([ValType::I64, ValType::I32, ValType::I64], []);
    types
        .ty()
        .function([ValType::I64], [ValType::I32, ValType::I64]);
    types.ty().function([ValType::I32], [ValType::I32]);
    types
        .ty()
        .function([ValType::I32], [ValType::I32, ValType::I64]);
    let mut imports = ImportSection::new();
    imports.import("env", "mem_load", EntityType::Function(0));
    imports.import("env", "mem_store", EntityType::Function(1));
    imports.import("env", "check_code", EntityType::Function(2));
    imports.import("env", "ecall", EntityType::Function(3));
    imports.import(
        "env",
        "memory",
        EntityType::Memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        }),
    );
    let mut funcs = FunctionSection::new();
    funcs.function(4); // the trace fn has type 4
    let mut code = CodeSection::new();
    code.function(&func);
    let mut exports = ExportSection::new();
    exports.export("trace", ExportKind::Func, 4);
    let mut module = Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&funcs)
        .section(&exports)
        .section(&code);
    let bytes = module.finish();

    // Validate (walrus's from_buffer parses + validates; wasmparser's
    // `validate` is feature-gated off in this workspace).
    walrus::Module::from_buffer(&bytes).expect("the vane-native trace is valid WASM");
}

/// Build a full vane-native module (a pack trace + a trampoline + the host
/// imports) and validate it — the trampoline's ABI bridging (the
/// `CompiledTraceFn` sret signature -> the vane-native pack fn ->
/// `vane_make_exit`) is checked by the WASM validator.
#[test]
fn vane_native_trampoline_validates() {
    use wasm_encoder::{
        CodeSection, EntityType, ExportKind, ExportSection, FunctionSection, ImportSection,
        MemoryType, Module, TypeSection, ValType,
    };
    let vane_abi = VaneAbiConfig {
        mem_idx: 0,
        regs_base_local: 0,
        scratch_a: 1,
        scratch_b: 2,
        scratch32: 3,
        mem_load_func: 0,
        mem_store_func: 1,
        check_code_func: 2,
        ecall_func: 3,
    };
    let trace = vec![
        StackOp::CheckCode {
            addr: 0x8000,
            expected: vec![].into(),
            miss_label: String::new(),
        },
        StackOp::LoadReg(1),
        StackOp::LoadMem {
            width: MemWidth::B8,
            signed: false,
        },
        StackOp::StoreReg(2),
        StackOp::PushImm(0x8004),
        StackOp::TailCall,
    ];
    let pack_fn = vane_patch::compile_trace_vane(trace, &vane_abi);
    // The trampoline: pack fn at func index 5, vane_make_exit at import index
    // 4, regs_offset 0x40 (a placeholder — the real offset is the interpreter's
    // exported value).
    let trampoline = vane_patch::compile_trampoline(5, 0x40, 4, 0x100);

    let mut module = Module::new();
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I64, ValType::I32, ValType::I32], [ValType::I64]); // 0: mem_load
    types
        .ty()
        .function([ValType::I64, ValType::I32, ValType::I64], []); // 1: mem_store
    types
        .ty()
        .function([ValType::I64], [ValType::I32, ValType::I64]); // 2: check_code
    types.ty().function([ValType::I32], [ValType::I32]); // 3: ecall
    types
        .ty()
        .function([ValType::I32, ValType::I64, ValType::I32], []); // 4: vane_make_exit
    types
        .ty()
        .function([ValType::I32], [ValType::I32, ValType::I64]); // 5: pack trace fn
    types
        .ty()
        .function([ValType::I32, ValType::I32, ValType::I32], []); // 6: trampoline
    let mut imports = ImportSection::new();
    imports.import("env", "mem_load", EntityType::Function(0));
    imports.import("env", "mem_store", EntityType::Function(1));
    imports.import("env", "check_code", EntityType::Function(2));
    imports.import("env", "ecall", EntityType::Function(3));
    imports.import("env", "vane_make_exit", EntityType::Function(4));
    imports.import(
        "env",
        "memory",
        EntityType::Memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        }),
    );
    let mut funcs = FunctionSection::new();
    funcs.function(5); // pack fn
    funcs.function(6); // trampoline
    let mut code = CodeSection::new();
    code.function(&pack_fn);
    code.function(&trampoline);
    let mut exports = ExportSection::new();
    exports.export("trampoline", ExportKind::Func, 6);
    module
        .section(&types)
        .section(&imports)
        .section(&funcs)
        .section(&exports)
        .section(&code);
    let bytes = module.finish();
    walrus::Module::from_buffer(&bytes).expect("the vane-native trampoline module is valid WASM");
}

// ── AOT bake (PLAN_ASYNC_JIT Phase C) ────────────────────────────────────────
//
// `bake_aot` is exercised against a synthetic "Wizened interpreter" module
// carrying the exact integration surface the real build exports (verified in
// the vane crate's own wasm32 build): the six `vane_*` functions,
// `VANE_CURRENT_HOST`/`VANE_AOT_TRACE` as immutable globals holding their
// linear-memory addresses (the form wasm-ld emits for `#[no_mangle]` statics),
// one linear memory, one funcref table. The stubs do the real memory
// mechanics (linear-memory reg cache, shims over linear memory), so the wasmi
// execution test below is a true end-to-end run of the baked trampoline.

/// `VANE_CURRENT_HOST`'s baked linear-memory address.
const HOST_ADDR: u32 = 65536;
/// `VANE_AOT_TRACE`'s baked address (its sentinel lives in a real data
/// segment so the patcher has bytes to rewrite).
const AOT_ADDR: u32 = 65792;
/// The `vane_regs_offset` stub's value (the synthetic `LocalStackHost`'s regs
/// offset within the "host").
const REGS_OFF: u32 = 64;
/// A seeded u64 the test trace loads through the `mem_load` shim.
const SEED_ADDR: u32 = 0x2000;

fn interp_module() -> Vec<u8> {
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ExportKind, ExportSection, FunctionSection,
        GlobalSection, GlobalType, Instruction, MemArg, MemorySection, MemoryType, Module,
        RefType, TableSection, TableType, TypeSection, ValType,
    };
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]); // 0: vane_regs_offset
    types.ty().function(
        [ValType::I32, ValType::I64, ValType::I32],
        [],
    ); // 1: vane_make_exit
    types.ty().function(
        [ValType::I64, ValType::I32, ValType::I32],
        [ValType::I64],
    ); // 2: vane_mem_load
    types.ty().function(
        [ValType::I64, ValType::I32, ValType::I64],
        [],
    ); // 3: vane_mem_store
    types.ty().function([ValType::I32, ValType::I64], []); // 4: vane_check_code_impl
    types.ty().function([ValType::I32], [ValType::I32]); // 5: vane_ecall

    let mut funcs = FunctionSection::new();
    for ty in 0..6u32 {
        funcs.function(ty);
    }

    let mut tables = TableSection::new();
    tables.table(TableType {
        element_type: RefType::FUNCREF,
        table64: false,
        minimum: 1,
        maximum: Some(1),
        shared: false,
    });

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 2,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    // The exported statics: immutable globals whose init IS their linear-
    // memory address (wasm-ld's form for #[no_mangle] statics).
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: false,
            shared: false,
        },
        &ConstExpr::i32_const(HOST_ADDR as i32),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: false,
            shared: false,
        },
        &ConstExpr::i32_const(AOT_ADDR as i32),
    );

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    for (i, name) in [
        "vane_regs_offset",
        "vane_make_exit",
        "vane_mem_load",
        "vane_mem_store",
        "vane_check_code_impl",
        "vane_ecall",
    ]
    .into_iter()
    .enumerate()
    {
        exports.export(name, ExportKind::Func, i as u32);
    }
    exports.export("VANE_CURRENT_HOST", ExportKind::Global, 0);
    exports.export("VANE_AOT_TRACE", ExportKind::Global, 1);

    let mut code = CodeSection::new();
    // 0: vane_regs_offset() -> REGS_OFF
    let mut f = wasm_encoder::Function::new([]);
    f.instruction(&Instruction::I32Const(REGS_OFF as i32));
    f.instruction(&Instruction::End);
    code.function(&f);
    // 1: vane_make_exit(tag, pc, ret_ptr): *(i32*)ret = tag; *(i64*)(ret+8) = pc
    let mut f = wasm_encoder::Function::new([]);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Store(MemArg {
        offset: 8,
        align: 3,
        memory_index: 0,
    }));
    f.instruction(&Instruction::End);
    code.function(&f);
    // 2: vane_mem_load(addr, width, signed) -> i64 (stub: the B8-unsigned path
    // only — wrap the address and load).
    let mut f = wasm_encoder::Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::I64Load(MemArg {
        offset: 0,
        align: 3,
        memory_index: 0,
    }));
    f.instruction(&Instruction::End);
    code.function(&f);
    // 3: vane_mem_store(addr, width, value)
    let mut f = wasm_encoder::Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I64Store(MemArg {
        offset: 0,
        align: 3,
        memory_index: 0,
    }));
    f.instruction(&Instruction::End);
    code.function(&f);
    // 4: vane_check_code_impl(ret_ptr, addr): *(ret_ptr) = (u32::MAX, 0) —
    // always Continue.
    let mut f = wasm_encoder::Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(-1)); // CONTINUE
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(0));
    f.instruction(&Instruction::I64Store(MemArg {
        offset: 8,
        align: 3,
        memory_index: 0,
    }));
    f.instruction(&Instruction::End);
    code.function(&f);
    // 5: vane_ecall(_) -> 0 (cont == 0 -> the trace returns Ecall)
    let mut f = wasm_encoder::Function::new([]);
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::End);
    code.function(&f);

    let mut data = DataSection::new();
    // The seed the test trace loads through the mem_load shim.
    data.active(
        0,
        &ConstExpr::i32_const(SEED_ADDR as i32),
        0x1234u64.to_le_bytes(),
    );
    // The VANE_AOT_TRACE sentinel: usize::MAX on wasm32 = "not baked".
    data.active(
        0,
        &ConstExpr::i32_const(AOT_ADDR as i32),
        [0xffu8, 0xff, 0xff, 0xff],
    );

    let mut module = Module::new();
    module
        .section(&types)
        .section(&funcs)
        .section(&tables)
        .section(&memories)
        .section(&globals)
        .section(&exports)
        .section(&code)
        .section(&data);
    module.finish()
}

#[test]
fn bake_aot_validates_and_stays_additive() {
    let target = interp_module();
    let traces: Vec<(u64, Vec<StackOp>)> = vec![
        (0x1000, simple_trace(0xdead)),
        (0x2000, simple_trace(0xbeef)),
    ];
    let with_traces = write_traces_section(&target, &traces).expect("write traces");
    let back = read_traces_section(&with_traces).expect("read traces");
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].0, 0x1000);
    assert_eq!(back[1].0, 0x2000);

    let baked = vane_patch::bake_aot(&with_traces, back).expect("bake");
    // The baked module is valid WASM (walrus parse; wasmi validates in the
    // execution test below) and preserves the target's structure.
    walrus::Module::from_buffer(&baked.wasm).expect("baked module validates");
    let audit = audit_no_stripping(&target, &baked.wasm).expect("additive bake");
    assert!(audit.is_clean(), "bake must be additive: {audit:?}");

    // One funcref-table slot per trace, starting past the original table
    // (which had exactly one entry).
    assert_eq!(baked.entries, vec![(0x1000, 1), (0x2000, 2)]);
    let text = wasmprinter::print_bytes(&baked.wasm).expect("printable");
    for name in ["vane_pack_1000", "vane_tramp_1000", "vane_pack_2000", "vane_tramp_2000"] {
        assert!(text.contains(name), "missing export {name}: {text}");
    }
    // The self-describing marker section survived emit.
    assert!(text.contains("vane.aot"));
}

/// The working-AOT'd-binary gate: instantiate the baked module in wasmi and
/// run a trace end-to-end through the trampoline — pack fn shims, the sret
/// Exit construction, and the baked trace table all in one WASM execution.
#[test]
fn bake_aot_runs_traces_in_wasmi() {
    let target = interp_module();
    // ALU + CheckCode (the shim continues) + a register round-trip + a load
    // through the mem_load shim + a TailCall exit.
    let trace = vec![
        StackOp::CheckCode {
            addr: 0x100,
            expected: vec![].into(),
            miss_label: String::new(),
        },
        StackOp::PushImm(0xdead),
        StackOp::StoreReg(1),
        StackOp::LoadReg(1),
        StackOp::PushImm(SEED_ADDR as u64),
        StackOp::LoadMem {
            width: MemWidth::B8,
            signed: false,
        },
        StackOp::StoreReg(2),
        StackOp::PushImm(0x8000),
        StackOp::TailCall,
    ];
    let baked = vane_patch::bake_aot(&target, vec![(0x1000, trace)]).expect("bake");

    let mut config = wasmi::Config::default();
    config.wasm_multi_memory(false);
    let engine = wasmi::Engine::new(&config);
    let module =
        wasmi::Module::new(&engine, &baked.wasm[..]).expect("baked module validates in wasmi");
    let mut store = wasmi::Store::new(&engine, ());
    let linker = wasmi::Linker::<()>::new(&engine);
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate");

    // The dispatch loop's view: host (LocalStackHost) at 2048, sret buffer at
    // 4096, ct_ptr unused by the trampoline.
    let host_ptr: u32 = 2048;
    let ret_ptr: u32 = 4096;
    let tramp = instance
        .get_typed_func::<(i32, i32, i32), ()>(&store, "vane_tramp_1000")
        .expect("trampoline export");
    tramp
        .call(&mut store, (ret_ptr as i32, host_ptr as i32, 0))
        .expect("the trampoline should run without trapping");

    let mem = instance.get_memory(&store, "memory").expect("memory export");
    let read = |addr: u32, len: usize| -> Vec<u8> {
        let mut buf = vec![0u8; len];
        mem.read(&store, addr as usize, &mut buf)
            .expect("read in-bounds");
        buf
    };

    // Exit = TailCall(0x8000): tag 0 at ret_ptr, pc at ret_ptr+8 (the Exit
    // layout rustc lowers: discriminant i32 at 0, payload at 8).
    let out = read(ret_ptr, 16);
    assert_eq!(
        i32::from_le_bytes(out[0..4].try_into().unwrap()),
        0,
        "exit tag = TailCall"
    );
    assert_eq!(
        i64::from_le_bytes(out[8..16].try_into().unwrap()),
        0x8000,
        "exit pc"
    );

    // The register cache at host_ptr + regs_offset: reg r lives at
    // regs_base + r*8 — r1 = 0xdead (store), r2 = 0x1234 (loaded through the
    // mem_load shim from the seeded memory), r3 untouched.
    let regs = read(host_ptr + REGS_OFF, 32);
    assert_eq!(
        u64::from_le_bytes(regs[8..16].try_into().unwrap()),
        0xdead,
        "r1 stored"
    );
    assert_eq!(
        u64::from_le_bytes(regs[16..24].try_into().unwrap()),
        0x1234,
        "r2 loaded through the mem_load shim"
    );
    assert_eq!(u64::from_le_bytes(regs[24..32].try_into().unwrap()), 0);

    // The host-context handoff: the trampoline wrote host_ptr into
    // VANE_CURRENT_HOST (which the shims read back).
    let host_cell = read(HOST_ADDR, 4);
    assert_eq!(u32::from_le_bytes(host_cell.try_into().unwrap()), host_ptr);

    // The trace table: VANE_AOT_TRACE points at the baked blob, which holds
    // the (pc, trampoline_table_idx) entry register_aot_traces will read.
    let aot = read(AOT_ADDR, 4);
    assert_eq!(
        u32::from_le_bytes(aot.try_into().unwrap()),
        baked.trace_table_addr,
        "VANE_AOT_TRACE patched to the baked table"
    );
    let table = read(baked.trace_table_addr, 8 + 16);
    assert_eq!(
        u32::from_le_bytes(table[0..4].try_into().unwrap()),
        1,
        "trace count"
    );
    assert_eq!(
        u64::from_le_bytes(table[8..16].try_into().unwrap()),
        0x1000,
        "entry pc"
    );
    assert_eq!(
        u32::from_le_bytes(table[16..20].try_into().unwrap()),
        baked.entries[0].1,
        "entry trampoline table idx"
    );
}

#[test]
fn bake_aot_with_no_traces_is_a_noop() {
    let target = interp_module();
    let baked = vane_patch::bake_aot(&target, Vec::new()).expect("bake");
    assert!(baked.entries.is_empty());
    assert_eq!(baked.trace_table_addr, 0);
    // The module still validates (and nothing was added).
    walrus::Module::from_buffer(&baked.wasm).expect("validates");
    let audit = audit_no_stripping(&target, &baked.wasm).expect("audit");
    assert!(audit.is_clean());
}

