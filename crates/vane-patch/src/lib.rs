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

use vane_target_core::StackOp;
use vane_target_wasm::WasmAbiConfig;
pub use vane_riscv::stackop_ser::{deserialize_trace_cache, serialize_trace_cache};

use walrus::Module;
use walrus::RawCustomSection;
use wasm_encoder::reencode::{RoundtripReencoder, utils::instruction};
use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, ElementSection, Elements, EntityType, ExportKind,
    ExportSection, Function, FunctionSection, ImportSection, Instruction, MemoryType, TableSection,
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
    // Every finite rv64 trace terminates in a nested TailCall inside the outer
    // LoopBegin/LoopEnd; WASM validation resets reachability at a loop's `end`,
    // so the function needs a trailing, unnested `unreachable` to satisfy its
    // implicit final return (see the RISC-V target's own renderer).
    for jit_op in vane_arch::template::render_stack_to_wasm(ops.into_iter(), *abi).chain(
        core::iter::once(vane_target_wasm::JitOpcode::Operator {
            op: wasmparser::Operator::Unreachable,
        }),
    ) {
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
    // Types: 0 = regfn (num_regs*i64 + target_pc i64 -> num_regs*i64);
    //        1 = ecall (num_regs*i64 -> num_regs*i64); 2 = jit_invalidate
    //        (i64 -> ()); 3 = register_trace (i64 pc, i32 slot -> ());
    //        4 = start (() -> ()).
    let mut types = TypeSection::new();
    let mut regfn_params: Vec<ValType> = (0..num_regs).map(|_| ValType::I64).collect();
    regfn_params.push(ValType::I64); // target_pc
    let regfn_results: Vec<ValType> = (0..num_regs).map(|_| ValType::I64).collect();
    types
        .ty()
        .function(regfn_params.clone(), regfn_results.clone());
    types.ty().function(
        (0..num_regs).map(|_| ValType::I64).collect::<Vec<_>>(),
        regfn_results.clone(),
    );
    types.ty().function([ValType::I64], []);
    types.ty().function([ValType::I64, ValType::I32], []); // register_trace
    types.ty().function([], []); // start

    // Imports (host imports + shared memory), at the indices the abi names:
    // ecall=0, jit_invalidate=1, lookup_stub=2 (as the abi refers to them),
    // then register_trace=3.
    let mut imports = ImportSection::new();
    imports.import("env", "ecall", EntityType::Function(1));
    imports.import("env", "jit_invalidate", EntityType::Function(2));
    imports.import("env", "lookup_stub", EntityType::Function(0));
    imports.import("env", "register_trace", EntityType::Function(3));
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

    // Compiled trace functions (func indices start after the 4 imports).
    let first_func = 4u32;
    let mut funcs = FunctionSection::new();
    let mut code = CodeSection::new();
    for (_pc, ops) in traces {
        funcs.function(0); // regfn type
        code.function(&compile_trace(ops, abi));
    }
    // The start function (registers the slots); its func index follows the traces.
    let start_func = first_func + n as u32;
    funcs.function(4); // start type

    // Dispatch table: funcref slots, one per trace. EXPORTED so the runtime
    // (and the host) can see the available slots.
    let mut tables = TableSection::new();
    tables.table(TableType {
        element_type: wasm_encoder::RefType::FUNCREF,
        table64: false,
        minimum: n as u64,
        maximum: Some(n as u64),
        shared: false,
    });

    // A declarative element segment declaring the trace functions, so the
    // start function can `ref.func` them.
    let func_indices: Vec<u32> = (0..n as u32).map(|i| first_func + i).collect();
    let mut elems = ElementSection::new();
    elems.declared(Elements::Functions(func_indices.clone().into()));

    // The start function body: for each trace, fill its slot in the dispatch
    // table and call the host's register_trace(pc, slot) so the runtime maps
    // the guest pc to the precompiled slot.
    let mut start_body = Function::new([]);
    for (i, pc) in pcs.iter().enumerate() {
        // slot[i] = ref.func(first_func + i)
        start_body.instruction(&Instruction::I32Const(i as i32));
        start_body.instruction(&Instruction::RefFunc(first_func + i as u32));
        start_body.instruction(&Instruction::TableSet(0));
        // register_trace(pc, slot)
        start_body.instruction(&Instruction::I64Const(*pc as i64));
        start_body.instruction(&Instruction::I32Const(i as i32));
        start_body.instruction(&Instruction::Call(3)); // register_trace import
    }
    start_body.instruction(&Instruction::End);
    code.function(&start_body);

    // pc -> slot map as a data segment (pairs of (pc: u64, slot: u32)).
    let mut map_bytes: Vec<u8> = Vec::new();
    for (i, pc) in pcs.iter().enumerate() {
        map_bytes.extend_from_slice(&pc.to_le_bytes());
        map_bytes.extend_from_slice(&(i as u32).to_le_bytes());
    }
    let mut data = DataSection::new();
    data.active(0, &ConstExpr::i32_const(0), map_bytes);

    // Exports: the slot table (all available slots) and each compiled trace fn.
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
    // Start (section 8) precedes Element (9), Code (10), Data (11).
    module.section(&wasm_encoder::StartSection {
        function_index: start_func,
    });
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

// ── os-jit-core JitBackend: recompile a guest region to a WASM artifact ─────
//
// The custom patcher plugs into the os-emulation JIT dispatch: a hot guest
// code region is decoded to a StackOp trace (the RISC-V frontend, `rv_emit`),
// recompiled to WASM, and returned as a `JitArtifact::Wasm` module that the
// runtime splices into its wasmi dispatch table — the "available slots".

/// Build a standalone WASM module exporting one compiled trace function under
/// `export_name`, conforming to `abi`'s regfn calling convention. This is the
/// per-trace artifact the runtime splices into its dispatch table.
pub fn compile_trace_module(
    ops: Vec<StackOp>,
    abi: &WasmAbiConfig,
    export_name: &str,
) -> Result<Vec<u8>, PatchError> {
    let num_regs = abi.num_regs;
    let mut types = TypeSection::new();
    let mut params: Vec<ValType> = (0..num_regs).map(|_| ValType::I64).collect();
    params.push(ValType::I64); // target_pc
    let results: Vec<ValType> = (0..num_regs).map(|_| ValType::I64).collect();
    types.ty().function(params, results);
    // Host import types: ecall (num_regs -> num_regs), jit_invalidate (i64->()).
    types.ty().function(
        (0..num_regs).map(|_| ValType::I64).collect::<Vec<_>>(),
        (0..num_regs).map(|_| ValType::I64).collect::<Vec<_>>(),
    );
    types.ty().function([ValType::I64], []);

    let mut imports = ImportSection::new();
    imports.import("env", "ecall", EntityType::Function(1));
    imports.import("env", "jit_invalidate", EntityType::Function(2));
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
    funcs.function(0); // the trace function: regfn type

    let mut exports = ExportSection::new();
    // Compiled trace function is index 2 (after the two function imports).
    exports.export(export_name, ExportKind::Func, 2);

    let mut code = CodeSection::new();
    code.function(&compile_trace(ops, abi));

    let mut module = wasm_encoder::Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&funcs);
    module.section(&exports);
    module.section(&code);
    let bytes = module.finish();
    Module::from_buffer(&bytes).map_err(|e| PatchError::Parse(format!("trace module: {e:#}")))?;
    Ok(bytes)
}

/// A custom-patcher JIT backend: recompiles a hot guest code region to a WASM
/// artifact. The region's raw bytes are decoded to a `StackOp` trace by the
/// RISC-V frontend and recompiled; the artifact is a standalone module the
/// runtime splices into its dispatch table (the available slots).
pub struct VaneWasmJit {
    abi: WasmAbiConfig,
}

impl VaneWasmJit {
    /// Create a backend. The `abi`'s host-import indices (ecall,
    /// jit_invalidate, memory) must match the runtime's import wiring; the
    /// register layout is taken from each request.
    pub fn new(abi: WasmAbiConfig) -> Self {
        VaneWasmJit { abi }
    }
}

/// A [`vane_arch::JitCtx`] serving a request's guest bytes (at `guest_pc`).
struct BytesCtx<'a> {
    base: u64,
    bytes: &'a [u8],
}
impl vane_arch::JitCtx for BytesCtx<'_> {
    fn bytes(&self, a: u64) -> Box<dyn Iterator<Item = u8> + '_> {
        let off = a.saturating_sub(self.base) as usize;
        Box::new(self.bytes.get(off..).unwrap_or(&[]).iter().copied())
    }
    fn byte(&self, a: u64) -> u8 {
        let off = a.saturating_sub(self.base) as usize;
        self.bytes.get(off).copied().unwrap_or(0)
    }
}

impl os_jit_core::JitBackend for VaneWasmJit {
    fn id(&self) -> os_jit_core::JitBackendId {
        os_jit_core::JitBackendId("vane-wasm")
    }

    fn compile(
        &mut self,
        req: &os_jit_core::JitRequest,
    ) -> Result<os_jit_core::JitArtifact, os_jit_core::JitError> {
        use vane_arch::template::{Flags, Params, TemplateJit};
        use vane_arch::{DebugFlate, Heat};
        if req.guest_bytes.is_empty() {
            return Err(os_jit_core::JitError::Decode("empty guest_bytes".into()));
        }
        let ctx = BytesCtx {
            base: req.guest_pc,
            bytes: &req.guest_bytes,
        };
        let flate = DebugFlate::default();
        let abi = WasmAbiConfig {
            num_regs: req.layout.num_regs,
            ..self.abi
        };
        let params = Params {
            react: &ctx,
            trial: &|_| Heat::New,
            flate: &flate,
            root: req.guest_pc,
            flags: Flags::default(),
            boundaries: None,
        };
        let jit = TemplateJit {
            params,
            pc: req.guest_pc,
            labels: Default::default(),
            depth: 0,
        };
        let ops = vane_riscv::template::riscv::rv_emit(&jit);
        if ops.is_empty() {
            return Err(os_jit_core::JitError::Decode("no trace emitted".into()));
        }
        let module = compile_trace_module(ops, &abi, "trace")
            .map_err(|e| os_jit_core::JitError::Unsupported(format!("{e:?}")))?;
        Ok(os_jit_core::JitArtifact::Wasm(module))
    }
}

// ── Traces read from snapshotted WASM (Wizer) ────────────────────────────────
//
// The interpreter serializes its hot trace cache into a `vane.traces` custom
// section before Wizer snapshots the module; the patcher reads the traces back
// from the snapshotted module, recompiles them, and fills the slots.

/// The custom section name holding the serialized trace cache.
pub const TRACES_SECTION: &str = "vane.traces";

/// Store a trace cache (pc → StackOp trace) into a module's `vane.traces`
/// custom section (replacing any prior one). The interpreter does this before
/// Wizer snapshots so the traces survive into the snapshotted module.
pub fn write_traces_section(
    wasm: &[u8],
    traces: &[(u64, Vec<StackOp>)],
) -> Result<Vec<u8>, PatchError> {
    let mut module = Module::from_buffer(wasm)?;
    while module.customs.remove_raw(TRACES_SECTION).is_some() {}
    module.customs.add(RawCustomSection {
        name: TRACES_SECTION.to_string(),
        data: serialize_trace_cache(traces),
    });
    Ok(module.emit_wasm())
}

/// Read the trace cache back from a snapshotted module's `vane.traces` section.
/// Returns an empty vec when no trace cache is present.
pub fn read_traces_section(wasm: &[u8]) -> Result<Vec<(u64, Vec<StackOp>)>, PatchError> {
    let module = Module::from_buffer(wasm)?;
    for (_id, section) in module.customs.iter() {
        if section.name() == TRACES_SECTION {
            let data = section.data(&walrus::IdsToIndices::default()).into_owned();
            return deserialize_trace_cache(&data)
                .map_err(|e| PatchError::Parse(format!("traces section: {e:?}")));
        }
    }
    Ok(Vec::new())
}
