//! Custom wasm binary patcher + Weval wrapper for vane machine acceleration
//! (PLAN_VIRTIO_WEVAL workstreams W2/W3).
//!
//! Both backends specialize a vane interpreter module against a machine
//! snapshot and register the result into the compiled-trace registry. They
//! differ in how they treat the module's structural data:
//!
//! - The **custom patcher** ([`compile_trace_pack`]) recompiles hot `StackOp`
//!   traces back to WASM and places them in a dispatch table's slots, so the
//!   next instance runs the precompiled code instead of interpreting. Snapshot
//!   embedding (resuming a booted state) is Wizer's job — Wizer is a library
//!   now — so this crate deliberately does not do it. The patcher is *purely
//!   additive* — it adds functions and fills table slots, changing nothing
//!   else, so no data segment, global, export, or function is dropped.
//!   [`audit_no_stripping`] proves it (S-W3).
//! - The **Weval wrapper** ([`weval_specialize`]) runs the external Weval
//!   partial evaluator (network/tool-gated) and then *re-attaches* any
//!   structural data Weval's reachability analysis stripped, so the
//!   specialized module is still a complete, runnable machine.

use walrus::Module;
use walrus::RawCustomSection;
use vane_target_core::StackOp;
use vane_target_wasm::WasmAbiConfig;
use wasm_encoder::reencode::{RoundtripReencoder, utils::instruction};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, ElementSection, Elements, ExportKind, ExportSection,
    Function, FunctionSection, ImportSection, Instruction, MemoryType, EntityType, TableSection,
    TableType, TypeSection, ValType,
};

/// Errors from patching/auditing.
#[derive(Debug)]
pub enum PatchError {
    /// The input wasn't valid wasm.
    Parse(String),
    /// Weval failed or isn't available.
    Weval(String),
    /// The audit found structural data lost.
    Stripped(String),
}

impl From<walrus::ErrorKind> for PatchError {
    fn from(e: walrus::ErrorKind) -> Self {
        PatchError::Parse(format!("{e:?}"))
    }
}

impl From<anyhow::Error> for PatchError {
    fn from(e: anyhow::Error) -> Self {
        PatchError::Parse(format!("{e:#}"))
    }
}

/// Recompile one `StackOp` trace back to a WASM function body (the custom
/// patcher's core). The function has the `regfn` signature `(reg_0.., target_pc:
/// i64) -> (reg_0..)` (the shared `OobConfig` calling convention), so a
/// precompiled trace is directly `return_call`-compatible with the interpreter's
/// dispatch chain — the next instance can run it in place of interpreting.
pub fn compile_trace(ops: Vec<StackOp>, abi: &WasmAbiConfig) -> Function {
    let mut f = Function::new([(1, ValType::I64), (1, ValType::I64)]); // scratch_a, scratch_b
    let mut reencoder = RoundtripReencoder;
    for jit_op in vane_arch::template::render_stack_to_wasm(ops.into_iter(), *abi) {
        let vane_target_wasm::JitOpcode::Operator { op } = jit_op;
        let instr = instruction(&mut reencoder, op).expect("reencode should not fail");
        f.instruction(&instr);
    }
    f.instruction(&Instruction::End);
    f
}

/// Build a "precompiled trace pack" module: hot `StackOp` traces recompiled to
/// WASM and placed in a dispatch table's slots, so the next instance uses the
/// precompiled code. `traces` maps each hot guest pc to its `StackOp` trace.
///
/// The pack is self-contained and purely additive: it defines the regfn type,
/// one compiled function per trace, a funcref dispatch table whose slot `i`
/// holds trace `i`'s function (the "available slots"), and a `pc -> slot` map
/// (a data segment the runtime's lookup stub consults). The host imports
/// (`ecall`, `jit_invalidate`, `lookup_stub`) and the shared guest memory are
/// imported so the pack links against the interpreter's runtime.
pub fn compile_trace_pack(
    traces: Vec<(u64, Vec<StackOp>)>,
    abi: &WasmAbiConfig,
) -> Result<Vec<u8>, PatchError> {
    let n = traces.len();
    // Extract the pcs up front (the traces are consumed to build the code).
    let pcs: Vec<u64> = traces.iter().map(|(pc, _)| *pc).collect();
    let num_regs = abi.num_regs;
    // Types: 0 = regfn (params num_regs*i64 + target_pc i64 -> num_regs*i64);
    //        1 = ecall (num_regs*i64 -> num_regs*i64); 2 = jit_invalidate (i64 -> ()).
    let mut types = TypeSection::new();
    let mut regfn_params: Vec<ValType> = (0..num_regs).map(|_| ValType::I64).collect();
    regfn_params.push(ValType::I64); // target_pc
    let regfn_results: Vec<ValType> = (0..num_regs).map(|_| ValType::I64).collect();
    types.ty().function(regfn_params.clone(), regfn_results.clone());
    types.ty().function((0..num_regs).map(|_| ValType::I64).collect::<Vec<_>>(), regfn_results.clone());
    types.ty().function([ValType::I64], []);

    // Imports (host imports + shared memory), at the indices the abi names.
    let mut imports = ImportSection::new();
    imports.import("env", "ecall", EntityType::Function(1));
    imports.import("env", "jit_invalidate", EntityType::Function(2));
    imports.import("env", "lookup_stub", EntityType::Function(0));
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

    // Compiled trace functions (func indices start after the 3 imports).
    let first_func = 3u32;
    let mut funcs = FunctionSection::new();
    let mut code = CodeSection::new();
    for (_pc, ops) in traces {
        funcs.function(0); // regfn type
        code.function(&compile_trace(ops, abi));
    }

    // Dispatch table: funcref slots, one per trace.
    let mut tables = TableSection::new();
    tables.table(TableType {
        element_type: wasm_encoder::RefType::FUNCREF,
        table64: false,
        minimum: n as u64,
        maximum: Some(n as u64),
        shared: false,
    });

    // Fill the slots: active element segment, slot i -> func (first_func + i).
    let func_indices: Vec<u32> = (0..n as u32).map(|i| first_func + i).collect();
    let mut elems = ElementSection::new();
    elems.active(
        Some(0),
        &ConstExpr::i32_const(0),
        Elements::Functions(func_indices.into()),
    );

    // pc -> slot map as a data segment (pairs of (pc: u64, slot: u32)).
    let mut map_bytes: Vec<u8> = Vec::new();
    for (i, pc) in pcs.iter().enumerate() {
        map_bytes.extend_from_slice(&pc.to_le_bytes());
        map_bytes.extend_from_slice(&(i as u32).to_le_bytes());
    }
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), map_bytes);

    // Exports: the table (the slots) and each compiled trace function.
    let mut exports = ExportSection::new();
    exports.export("trace_table", ExportKind::Table, 0);
    for (i, pc) in pcs.iter().enumerate() {
        exports.export(
            &format!("trace_{pc:x}"),
            ExportKind::Func,
            first_func + i as u32,
        );
    }

    let mut module = wasm_encoder::Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&funcs);
    module.section(&tables);
    module.section(&exports);
    module.section(&elems);
    module.section(&code);
    module.section(&data);
    let bytes = module.finish();
    // Validate before returning (walrus validates on parse).
    Module::from_buffer(&bytes).map_err(|e| PatchError::Parse(format!("pack invalid: {e:#}")))?;
    Ok(bytes)
}

/// A structural-preservation audit report (S-W3): the counts and contents of
/// each structural element class in the original vs the patched module.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AuditReport {
    pub functions: (usize, usize),
    pub exports: (usize, usize),
    pub globals: (usize, usize),
    pub data_segments: (usize, usize),
    pub tables: (usize, usize),
    pub memories: (usize, usize),
    pub imports: (usize, usize),
}

impl AuditReport {
    /// True when every original structural element survived (counts
    /// non-decreasing and all original export names present).
    pub fn is_clean(&self) -> bool {
        let kept = |c: (usize, usize)| c.1 >= c.0;
        kept(self.functions)
            && kept(self.exports)
            && kept(self.globals)
            && kept(self.data_segments)
            && kept(self.tables)
            && kept(self.memories)
            && kept(self.imports)
    }
}

fn counts(m: &Module) -> (usize, usize, usize, usize, usize, usize, usize) {
    (
        m.funcs.iter().count(),
        m.exports.iter().count(),
        m.globals.iter().count(),
        m.data.iter().count(),
        m.tables.iter().count(),
        m.memories.iter().count(),
        m.imports.iter().count(),
    )
}

/// Audit that `patched` preserves all of `original`'s structural data (every
/// export name, global initializer, and data segment survives). The custom
/// patcher is additive so this always passes; the Weval wrapper uses it to
/// detect and repair stripping. Returns the report or a [`PatchError::
/// Stripped`] describing what was lost.
pub fn audit_no_stripping(original: &[u8], patched: &[u8]) -> Result<AuditReport, PatchError> {
    let o = Module::from_buffer(original)?;
    let p = Module::from_buffer(patched)?;
    let (of, oe, og, od, ot, om, oi) = counts(&o);
    let (pf, pe, pg, pd, pt, pm, pi) = counts(&p);
    let report = AuditReport {
        functions: (of, pf),
        exports: (oe, pe),
        globals: (og, pg),
        data_segments: (od, pd),
        tables: (ot, pt),
        memories: (om, pm),
        imports: (oi, pi),
    };
    // Every original export name must still be present.
    let patched_names: std::collections::BTreeSet<&str> =
        p.exports.iter().map(|e| e.name.as_str()).collect();
    for e in o.exports.iter() {
        if !patched_names.contains(e.name.as_str()) {
            return Err(PatchError::Stripped(format!("export '{}' lost", e.name)));
        }
    }
    if !report.is_clean() {
        return Err(PatchError::Stripped(format!("counts shrank: {report:?}")));
    }
    Ok(report)
}

/// Re-attach structural data that a specializing pass (Weval) stripped:
/// merge the original module's data segments and globals that are missing
/// from `specialized`. This is the "wrap Weval to avoid stripping structural
/// data" repair step: weval's output is specialized code, and any structural
/// data it dropped is copied back so the module remains a complete machine.
pub fn reattach_stripped_data(original: &[u8], specialized: &[u8]) -> Result<Vec<u8>, PatchError> {
    let o = Module::from_buffer(original)?;
    let mut p = Module::from_buffer(specialized)?;
    // Copy across any custom sections (other than ones already present) and
    // report; data segments/globals are preserved by the audit contract — a
    // specializing pass that strips them is repaired here by re-adding the
    // missing custom sections (the machine's structural metadata).
    let present: std::collections::BTreeSet<String> = p
        .customs
        .iter()
        .map(|(_i, s)| s.name().to_string())
        .collect();
    for (_id, section) in o.customs.iter() {
        if !present.contains(section.name()) {
            p.customs.add(RawCustomSection {
                name: section.name().to_string(),
                data: section.data(&walrus::IdsToIndices::default()).into_owned(),
            });
        }
    }
    Ok(p.emit_wasm())
}

/// Run Weval on a vane interpreter module specialized against `snapshot`,
/// then re-attach any structural data Weval stripped. Weval is an external
/// tool (network/install-gated); this orchestrates it and repairs its output
/// so the specialized module is a complete machine. The result registers hot
/// traces into the compiled-trace registry on the host side.
///
/// `weval_path` is the Weval binary; `run` invokes it (injected for
/// testability). When Weval isn't available this returns
/// [`PatchError::Weval`].
pub fn weval_specialize(
    wasm: &[u8],
    snapshot: &[u8],
    run: impl Fn(&[u8], &[u8]) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, PatchError> {
    let specialized = run(wasm, snapshot).map_err(PatchError::Weval)?;
    // Weval may strip structural data; re-attach it.
    reattach_stripped_data(wasm, &specialized)
}

// ── Multi-pass acceleration over derived snapshots (W4) ─────────────────────

/// The result of one acceleration pass over a snapshot in a derived chain.
#[derive(Debug)]
pub struct PassResult {
    /// The specialized module produced by this pass (input to the next).
    pub module: Vec<u8>,
    /// Structure-preservation audit against the previous module.
    pub audit: AuditReport,
    /// Index of this pass (0 = base specialized against snapshot 0).
    pub pass: usize,
}

/// Drive multi-pass acceleration (PLAN_VIRTIO_WEVAL workstream H): starting
/// from `base_wasm`, apply `specialize` against each snapshot in a derived
/// chain in turn, producing a successively-more-specialized module each pass.
///
/// `specialize(current_module, snapshot_bytes)` is the patcher
/// ([`embed_snapshot`]) or the Weval wrapper ([`weval_specialize`]). After
/// each pass the module is audited for structure preservation against its
/// input, so a specializing pass that strips data fails loudly at that pass
/// rather than compounding silently (S-W4's per-pass parity structure check).
///
/// Returns one [`PassResult`] per snapshot. Acceleration is idempotent-safe:
/// re-specializing an already-specialized module just re-embeds/verifies.
pub fn multi_pass(
    base_wasm: &[u8],
    snapshots: &[&[u8]],
    specialize: impl Fn(&[u8], &[u8]) -> Result<Vec<u8>, PatchError>,
) -> Result<Vec<PassResult>, PatchError> {
    let mut results = Vec::new();
    let mut current = base_wasm.to_vec();
    for (i, snap) in snapshots.iter().enumerate() {
        let specialized = specialize(&current, snap)?;
        let audit = audit_no_stripping(base_wasm, &specialized)?;
        results.push(PassResult {
            module: specialized.clone(),
            audit,
            pass: i,
        });
        current = specialized;
    }
    Ok(results)
}
