//! W3 (custom patcher) + W2 (Weval wrapper): embed/extract round-trip,
//! structure-preservation audit, and the weval-strip repair.
use vane_patch::{
    audit_no_stripping, embed_snapshot, extract_snapshot, weval_specialize, SNAPSHOT_SECTION,
};

/// Build a minimal but structurally non-trivial wasm module: a memory, a
/// global, an exported function, and a data segment.
fn sample_module() -> Vec<u8> {
    use wasm_encoder::*;
    let mut m = Module::new();
    // (type (func (result i32)))
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    m.section(&types);
    // (func (type 0) ...)
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    m.section(&funcs);
    // memory
    let mut mems = MemorySection::new();
    mems.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    m.section(&mems);
    // global
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: false,
            shared: false,
        },
        &ConstExpr::i32_const(7),
    );
    m.section(&globals);
    // (export "answer" (func 0)) and (export "memory" (memory 0))
    let mut exports = ExportSection::new();
    exports.export("answer", ExportKind::Func, 0);
    exports.export("memory", ExportKind::Memory, 0);
    m.section(&exports);
    // code: the function body returns 42 (section 10, before data)
    let mut code = CodeSection::new();
    let mut f = Function::new([]);
    f.instruction(&Instruction::I32Const(42));
    f.instruction(&Instruction::End);
    code.function(&f);
    m.section(&code);
    // data segment "structural data" (section 11, last)
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), b"structural".iter().copied());
    m.section(&data);
    m.finish()
}

#[test]
fn embed_extract_roundtrip() {
    let wasm = sample_module();
    let snapshot = b"SNAPSHOT-BYTES-\x00\x01\x02";
    let patched = embed_snapshot(&wasm, snapshot).expect("embed");
    let got = extract_snapshot(&patched).expect("extract");
    assert_eq!(got.as_deref(), Some(&snapshot[..]));
}

#[test]
fn patcher_strips_nothing() {
    let wasm = sample_module();
    let patched = embed_snapshot(&wasm, b"snap").expect("embed");
    // S-W3: every original structural element survives (export names, counts).
    let report = audit_no_stripping(&wasm, &patched).expect("audit clean");
    assert!(report.is_clean());
    assert_eq!(report.exports.0, report.exports.1); // no export lost
    assert_eq!(report.data_segments.0, report.data_segments.1);
}

#[test]
fn weval_wrapper_reattaches_stripped_data() {
    let wasm = sample_module();
    // Add a structural custom section that Weval would strip.
    let with_meta = embed_snapshot(&wasm, b"meta").expect("embed");
    // Simulate Weval: it returns the module WITHOUT the custom section
    // (reachability stripped it).
    let fake_weval = |w: &[u8], _snap: &[u8]| -> Result<Vec<u8>, String> {
        // "Strip" = re-emit without the vane.snapshot section. walrus drops
        // unregistered custom sections on a fresh parse only if we re-encode
        // selectively; here we approximate stripping by rebuilding from the
        // original (which has no snapshot section).
        let _ = w;
        Ok(sample_module())
    };
    let out = weval_specialize(&with_meta, b"snap", fake_weval).expect("weval");
    // The wrapper re-attached the stripped snapshot section.
    let got = extract_snapshot(&out).expect("extract");
    assert_eq!(got.as_deref(), Some(&b"meta"[..]), "structural data restored");
    // And the specialized code's own structure is intact.
    assert!(audit_no_stripping(&wasm, &out).expect("audit").is_clean());
}

#[test]
fn weval_unavailable_is_a_clean_error() {
    let wasm = sample_module();
    let err = weval_specialize(&wasm, b"snap", |_w, _s| Err("weval not installed".into()));
    assert!(matches!(err, Err(vane_patch::PatchError::Weval(_))));
    let _ = SNAPSHOT_SECTION;
}
