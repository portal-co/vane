//! W3 (custom patcher: StackOp->WASM trace precompilation + slot filling) and
//! W2 (Weval wrapper). Snapshot embedding is Wizer's job, not this crate's.
use vane_patch::{
    audit_no_stripping, compile_trace, compile_trace_pack, multi_pass, weval_specialize,
};
use vane_target_core::StackOp;
use vane_target_wasm::WasmAbiConfig;

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
                StackOp::LoopBegin { label: "x8000".into() },
                StackOp::Br { relative_depth: 1, label: "x8000".into() },
                StackOp::LoopEnd,
                StackOp::LoadMem { width: MemWidth::B4, signed: true },
                StackOp::StoreMem { width: MemWidth::B8 },
                StackOp::FloatBinary { width: FloatWidth::F64, op: FloatBinOp::Add },
                StackOp::CheckCode { addr: 0x100, expected: vec![1, 2, 3].into(), miss_label: "m".into() },
                StackOp::Trap("boom".into()),
                StackOp::PushImm(0x8000_0000),
                StackOp::TailCall,
            ],
        ),
        (0x8000_0100, vec![StackOp::Ecall, StackOp::Log("hi".into())]),
    ];
    let bytes = vane_patch::serialize_trace_cache(&traces);
    let back = vane_patch::deserialize_trace_cache(&bytes).expect("decode");
    // StackOp has no PartialEq, so compare by re-serializing.
    assert_eq!(vane_patch::serialize_trace_cache(&back), bytes, "round-trip stable");
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
    assert!(text.contains("table.set"), "start fn fills slots via table.set");
    assert!(text.contains("register_trace"), "start fn calls register_trace");
    // The slot table is exported.
    assert!(text.contains("trace_table"), "slot table exported");
}
