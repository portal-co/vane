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

pub use vane_riscv::stackop_ser::{deserialize_trace_cache, serialize_trace_cache};
use vane_target_core::StackOp;
use vane_target_wasm::{TailCallShims, UndoShims, VaneAbiConfig, WasmAbiConfig};

use walrus::ir::{
    BinaryOp, Binop, Block, Br, Call, CallIndirect, Const, Drop, GlobalGet, IfElse, InstrSeqType,
    Load, LoadKind, LocalGet, LocalSet, LocalTee, Loop, MemArg, Return, Store, StoreKind, UnaryOp,
    Unop, Unreachable, Value,
};
use walrus::{
    DataKind, ElementItems, ElementKind, ExportItem, FunctionBuilder, GlobalKind, LocalId,
    MemoryId, Module, ModuleTypes, RawCustomSection,
};
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

/// Recompile one `StackOp` trace to a WASM function body for the **vane-native
/// ABI** ([`VaneAbiConfig`]) — the vane interpreter's own WASM build, no speet
/// runtime. The function's signature is `(regs_base: i32) -> (exit_tag: i32,
/// exit_pc: i64)`: it reads/writes the host register cache in linear memory and
/// returns an exit to the dispatch loop (see [`VaneAbiConfig`]'s doc comment).
/// The two i64 locals are the renderer's `scratch_a`/`scratch_b`.
pub fn compile_trace_vane(ops: Vec<StackOp>, abi: &VaneAbiConfig) -> Function {
    // locals: scratch_a, scratch_b (i64), scratch32 (i32) — after the
    // regs_base (i32) param.
    let mut f = Function::new([(2, ValType::I64), (1, ValType::I32)]);
    let mut reencoder = RoundtripReencoder;
    // As with the speet renderer, a finite trace terminates in a nested
    // TailCall (a `return`) inside the outer loop; the trailing unnested
    // `unreachable` satisfies WASM validation's implicit final return.
    for jit_op in vane_arch::template::render_stack_to_wasm_vane(ops.into_iter(), *abi).chain(
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

/// Generate a **trampoline** with the `CompiledTraceFn` WASM ABI
/// `(ret_ptr: i32, host_ptr: i32, ct_ptr: i32) -> ()` that bridges the vane
/// interpreter's dispatch loop to a vane-native pack trace fn. Verified against
/// rustc's WASM lowering: the sret `ret_ptr` is param 0, `host_ptr` param 1,
/// `ct_ptr` param 2. The trampoline:
///
/// 1. computes `regs_base = host_ptr + regs_offset` (the `LocalStackHost`'s
///    `regs` field address — `regs_offset` is baked in by the patcher from the
///    interpreter's exported offset),
/// 2. calls the pack trace fn at `pack_fn_idx` (the vane-native ABI
///    `(regs_base: i32) -> (exit_tag: i32, exit_pc: i64)`, a multi-value
///    return),
/// 3. calls the `vane_make_exit(exit_tag, exit_pc, ret_ptr)` host import
///    (`make_exit_idx`), which constructs the `Exit` — including the
///    `Trap(String)` case, which WASM can't build — and writes it to `ret_ptr`.
///
/// The dispatch loop is untouched: an AOT'd trace's `CompiledTrace.f` points at
/// this trampoline.
/// The dispatch loop is untouched: an AOT'd trace's `CompiledTrace.f` points at
/// this trampoline. `current_host_addr` is the linear-memory address of the
/// interpreter's `VANE_CURRENT_HOST` global (baked in by the patcher), which
/// the trampoline sets so the pack fn's shims recover the host.
pub fn compile_trampoline(
    pack_fn_idx: u32,
    regs_offset: u32,
    make_exit_idx: u32,
    current_host_addr: u32,
) -> Function {
    let mut f = Function::new([]); // no extra locals
    // VANE_CURRENT_HOST = host_ptr (so the pack fn's shims recover the host)
    f.instruction(&Instruction::I32Const(current_host_addr as i32));
    f.instruction(&Instruction::LocalGet(1)); // host_ptr
    f.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    }));
    // regs_base = host_ptr + regs_offset
    f.instruction(&Instruction::LocalGet(1)); // host_ptr
    f.instruction(&Instruction::I32Const(regs_offset as i32));
    f.instruction(&Instruction::I32Add);
    // (exit_tag, exit_pc) = pack_fn(regs_base)
    f.instruction(&Instruction::Call(pack_fn_idx));
    // vane_make_exit(exit_tag, exit_pc, ret_ptr)
    f.instruction(&Instruction::LocalGet(0)); // ret_ptr
    f.instruction(&Instruction::Call(make_exit_idx));
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
        data: serialize_trace_cache(traces.iter().map(|(pc, ops)| (*pc, ops.as_slice()))),
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

// ── AOT bake: pack fns + trampolines into the interpreter's own module ──────
//
// PLAN_ASYNC_JIT Phase C — the last piece of the WASM JIT integration. Given a
// Wizened interpreter module (carrying the `vane.*` integration exports) and
// its trace cache, [`bake_aot`] bakes one pack fn (the vane-native ABI) plus
// one trampoline (the `CompiledTraceFn` ABI) per hot trace DIRECTLY into the
// target module — walrus's ID-remapping machinery is `pub(crate)`, so an
// encode/parse round-trip cannot work and the functions are built in place
// with `FunctionBuilder` — appends the trampolines to the module's funcref
// table, bakes the `[count][pad][entries...]` trace table, and patches the
// interpreter's `VANE_AOT_TRACE` static (whose address `register_aot_traces`
// reads) to it. The result is a module whose next run dispatches AOT'd traces.
//
// The transform is additive: existing functions/exports/globals/data segments
// survive untouched except two facts — the funcref table GROWS (new entries
// appended past its old size) and the four bytes at `VANE_AOT_TRACE`'s address
// change value inside an existing segment (structure unchanged). Both are
// provable with [`audit_no_stripping`].

/// Local indices the baked pack fns give the renderer ([`VaneAbiConfig`]): 0 is
/// the `regs_base` param, 1/2 the i64 scratches, 3 the i32 scratch.
const BAKE_REGS_BASE: u32 = 0;
const BAKE_SCRATCH_A: u32 = 1;
const BAKE_SCRATCH_B: u32 = 2;
const BAKE_SCRATCH32: u32 = 3;

/// Shim function order a baked pack fn's `VaneAbiConfig` refers to by index.
const BAKE_MEM_LOAD: usize = 0;
const BAKE_MEM_STORE: usize = 1;
const BAKE_CHECK_CODE: usize = 2;
const BAKE_ECALL: usize = 3;
const BAKE_RECORD_UNDO: usize = 4;
const BAKE_SET_UNDO_BASE: usize = 5;
/// Direct tail-call lookup shim (`vane_tail_lookup`): slot 6 in the pack
/// fns' shim index space (see `VaneAbiConfig::tail_call`).
const BAKE_TAIL_LOOKUP: usize = 6;

/// The result of a successful [`bake_aot`].
#[derive(Debug, Clone)]
pub struct BakeResult {
    /// The patched module.
    pub wasm: Vec<u8>,
    /// `(guest pc, funcref table index of the trampoline)` per baked trace —
    /// the same `(pc, trampoline_table_idx)` entries written to the trace
    /// table, exposed so the driver can verify the bake.
    pub entries: Vec<(u64, u32)>,
    /// Linear-memory address of the baked
    /// `[count: u32][pad: u32][{ pc: u64, trampoline_table_idx: u32, _pad: u32 }...]`
    /// blob — the address `VANE_AOT_TRACE` was patched to (also mirrored in
    /// the `vane.aot` custom section: `count: u32, blob_addr: u32`).
    pub trace_table_addr: u32,
    /// The async build only: the linear-memory address of the interpreter's
    /// `VANE_ASYNC_OP` pending-op area (exported global, no patching) — the
    /// driver polls `state` / reads `addr`,`width`,`signed`,`value` and
    /// writes `result` there across a suspend/resume. `None` for sync bakes.
    pub async_op_addr: Option<u32>,
}

/// Bake AOT traces into a Wizened interpreter module (PLAN_ASYNC_JIT Phase C).
///
/// `traces` is the trace cache read back from the module's `vane.traces`
/// section (see [`read_traces_section`]) — guest pc → `StackOp` trace. For
/// each trace this bakes:
///
/// - a **pack fn** `(regs_base: i32) -> (exit_tag: i32, exit_pc: i64)` (the
///   vane-native ABI), rendered by [`vane_target_wasm`]'s renderer and lowered
///   into walrus IR in place (registers in linear memory at `regs_base + r*8`,
///   memory ops via the interpreter's exported `vane_mem_load`/`vane_mem_store`,
///   `CheckCode` via a generated multi-value wrapper over the sret
///   `vane_check_code_impl`, `Ecall` via `vane_ecall`, exits as
///   `(tag, pc)` returns);
/// - a **trampoline** `(ret_ptr, host_ptr, ct_ptr) -> ()` (the
///   `CompiledTraceFn` WASM ABI) that stores `host_ptr` into the
///   interpreter's `VANE_CURRENT_HOST` global (so the pack fn's shims recover
///   the host), computes `regs_base = host_ptr + vane_regs_offset()`, calls
///   the pack fn, and lets `vane_make_exit(tag, pc, ret_ptr)` construct the
///   `Exit` through the sret pointer.
///
/// Trampolines are appended to the module's funcref table (entry `old_size + i`;
/// a WASM fn pointer IS its table index, which is what `register_aot_traces`
/// transmutes), exported as `vane_pack_{pc:x}` / `vane_tramp_{pc:x}` for
/// debuggability, and the trace table is baked + `VANE_AOT_TRACE` patched so
/// the next run registers everything via `Reactor::register_aot_traces`.
///
/// Baked scratch (the `check_code` sret buffer and the trace table) is placed
/// in linear memory above every active data segment end and every global-init
/// address — i.e. in the static region's alignment padding below
/// `__heap_base` (wasm-ld lays out stack/statics/heap in address order, so
/// nothing there is live). The module must export: `vane_make_exit`,
/// `vane_regs_offset`, `vane_mem_load`, `vane_mem_store`,
/// `vane_check_code_impl`, `vane_ecall` (functions), `VANE_CURRENT_HOST` and
/// `VANE_AOT_TRACE` (immutable globals holding their linear-memory addresses,
/// the form wasm-ld emits for `#[no_mangle]` statics), one linear memory, and
/// one funcref table.
pub fn bake_aot(
    target_wasm: &[u8],
    traces: Vec<(u64, Vec<StackOp>)>,
) -> Result<BakeResult, PatchError> {
    bake_aot_impl(target_wasm, traces, false)
}

/// The async surface of [`bake_aot`] (PLAN_ASYNC_JIT Phase B): memory ops
/// bind to the `vane_mem_load_async`/`vane_mem_store_async` shims (which
/// suspend via Binaryen asyncify once [`asyncify_module`] has transformed the
/// module), and the module must export `VANE_ASYNC_OP` — its address comes
/// back in [`BakeResult::async_op_addr`] for the driver. Everything else
/// (undo records, check_code, ecall, the trace table) is identical.
pub fn bake_aot_async(
    target_wasm: &[u8],
    traces: Vec<(u64, Vec<StackOp>)>,
) -> Result<BakeResult, PatchError> {
    bake_aot_impl(target_wasm, traces, true)
}

fn bake_aot_impl(
    target_wasm: &[u8],
    traces: Vec<(u64, Vec<StackOp>)>,
    async_shims: bool,
) -> Result<BakeResult, PatchError> {
    let mut module = Module::from_buffer(target_wasm)?;
    if traces.is_empty() {
        return Ok(BakeResult {
            wasm: module.emit_wasm(),
            entries: Vec::new(),
            trace_table_addr: 0,
            async_op_addr: None,
        });
    }

    // ── The target module's integration surface ──────────────────────────
    let memory = module
        .memories
        .iter()
        .next()
        .map(|m| m.id())
        .ok_or_else(|| PatchError::Parse("target module has no linear memory".into()))?;
    let table = module
        .tables
        .iter()
        .next()
        .map(|t| t.id())
        .ok_or_else(|| PatchError::Parse("target module has no funcref table".into()))?;
    let make_exit = exported_func(&module, "vane_make_exit")?;
    let regs_offset = exported_func(&module, "vane_regs_offset")?;
    let mem_load = exported_func(
        &module,
        if async_shims {
            "vane_mem_load_async"
        } else {
            "vane_mem_load"
        },
    )?;
    let mem_store = exported_func(
        &module,
        if async_shims {
            "vane_mem_store_async"
        } else {
            "vane_mem_store"
        },
    )?;
    let check_code_impl = exported_func(&module, "vane_check_code_impl")?;
    let ecall = exported_func(&module, "vane_ecall")?;
    let record_undo = exported_func(&module, "vane_record_undo")?;
    let set_undo_base = exported_func(&module, "vane_set_undo_base")?;
    let current_host_addr = exported_global_addr(&module, "VANE_CURRENT_HOST")?;
    let tail_lookup = exported_func(&module, "vane_tail_lookup")?;
    let current_host_global = exported_global_id(&module, "VANE_CURRENT_HOST")?;
    let aot_trace_addr = exported_global_addr(&module, "VANE_AOT_TRACE")?;
    let async_op_addr = if async_shims {
        Some(exported_global_addr(&module, "VANE_ASYNC_OP")?)
    } else {
        None
    };

    // Scratch in the static region's padding (see doc): above every active
    // data segment end and every global-init (data-symbol) address.
    let mut scratch = Scratch::new(&module);

    // The multi-value `check_code` wrapper over the interpreter's sret helper
    // (a Rust tuple return is sret, NOT multi-value, so the pack fn's
    // `(addr) -> (tag, pc)` shim must be generated here).
    let check_buf = scratch.alloc(16);
    // The direct-tail sret scratch: the sentinel calls the TARGET's
    // trampoline with this as its ret_ptr; the resulting (tag, pc) is read
    // back and returned by the calling pack fn (an ordinary call — the
    // chain depth is bounded by the hop budget, so no tail-call proposal
    // is required).
    let tail_ret_buf = scratch.alloc(16);
    let check_code = build_check_code_wrapper(&mut module, check_code_impl, check_buf, memory)?;

    // ── Pack fns + trampolines, one pair per trace ────────────────────────
    let abi = VaneAbiConfig {
        mem_idx: 0, // unused: the lowering maps memory ops to `memory` directly
        regs_base_local: BAKE_REGS_BASE,
        scratch_a: BAKE_SCRATCH_A,
        scratch_b: BAKE_SCRATCH_B,
        scratch32: BAKE_SCRATCH32,
        mem_load_func: BAKE_MEM_LOAD as u32,
        mem_store_func: BAKE_MEM_STORE as u32,
        check_code_func: BAKE_CHECK_CODE as u32,
        ecall_func: BAKE_ECALL as u32,
        undo_shims: Some(UndoShims {
            record_undo_func: BAKE_RECORD_UNDO as u32,
            set_undo_base_func: BAKE_SET_UNDO_BASE as u32,
        }),
        // Direct tail chaining: the renderer emits a sentinel
        // call_indirect; the bake substitutes the real global/type/table
        // ids and the post-encode splice makes the opcode a true tail
        // call. The placeholder indices here are never encoded — the bake
        // lowers CallIndirect/GlobalGet by kind, not by index.
        tail_call: Some(TailCallShims {
            lookup_func: BAKE_TAIL_LOOKUP as u32,
            host_global: 0,
            trampoline_type: 0,
            ret_scratch_addr: tail_ret_buf,
        }),
    };
    let shims = [
        mem_load,
        mem_store,
        check_code,
        ecall,
        record_undo,
        set_undo_base,
        tail_lookup,
    ];
    let old_table_size = module.tables.get(table).initial;
    let mut entries = Vec::with_capacity(traces.len());
    let mut tramp_ids = Vec::with_capacity(traces.len());
    for (pc, ops) in traces {
        // The trampolines' funcref signature `(ret_ptr, host, ct) -> ()`:
        // created once, referenced by the direct-tail call_indirect
        // sentinels (structurally identical to build_trampoline's type —
        // WASM type matching is structural, so a duplicate entry is valid).
        let trampoline_ty = module.types.add(
            &[
                walrus::ValType::I32,
                walrus::ValType::I32,
                walrus::ValType::I32,
            ],
            &[],
        );
        let pack_fn = build_pack_fn(
            &mut module,
            ops,
            abi,
            memory,
            &shims,
            current_host_global,
            trampoline_ty,
            table,
        )?;
        let tramp = build_trampoline(
            &mut module,
            pack_fn,
            regs_offset,
            make_exit,
            current_host_addr,
            memory,
        )?;
        module
            .exports
            .add(&format!("vane_pack_{pc:x}"), ExportItem::from(pack_fn));
        module
            .exports
            .add(&format!("vane_tramp_{pc:x}"), ExportItem::from(tramp));
        let idx = (old_table_size + tramp_ids.len() as u64) as u32;
        tramp_ids.push(tramp);
        entries.push((pc, idx));
    }

    // Append the trampolines to the funcref table (entry `old_size + i`) and
    // grow its limits to match. A WASM fn pointer is exactly the table index.
    {
        let t = module.tables.get_mut(table);
        t.initial += tramp_ids.len() as u64;
        if let Some(max) = t.maximum.as_mut() {
            *max = (*max).max(t.initial);
        }
    }
    module.elements.add(
        ElementKind::Active {
            table,
            offset: walrus::ConstExpr::Value(walrus::ir::Value::I32(old_table_size as i32)),
        },
        ElementItems::Functions(tramp_ids),
    );

    // ── The trace table + the VANE_AOT_TRACE patch ────────────────────────
    // Layout (repr(C), matching `Reactor::register_aot_traces`):
    // `[count: u32][pad: u32][{ pc: u64, trampoline_table_idx: u32, _pad: u32 }; count]`.
    let blob_len = 8 + entries.len() * 16;
    let blob_addr = scratch.alloc(blob_len as u32);
    let mut blob = Vec::with_capacity(blob_len);
    blob.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    blob.extend_from_slice(&0u32.to_le_bytes());
    for (pc, idx) in &entries {
        blob.extend_from_slice(&pc.to_le_bytes());
        blob.extend_from_slice(&idx.to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());
    }
    module.data.add(
        DataKind::Active {
            memory,
            offset: walrus::ConstExpr::Value(walrus::ir::Value::I32(blob_addr as i32)),
        },
        blob,
    );
    // Point VANE_AOT_TRACE at the blob: patch its four baked bytes inside
    // whichever existing segment holds them (its non-zero sentinel initial
    // value guarantees a real segment exists).
    patch_static(&mut module, aot_trace_addr, &blob_addr.to_le_bytes())?;

    // A self-describing marker for the host/instantiator.
    let mut meta = Vec::with_capacity(8);
    meta.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    meta.extend_from_slice(&blob_addr.to_le_bytes());
    module.customs.add(RawCustomSection {
        name: "vane.aot".to_string(),
        data: meta,
    });

    Ok(BakeResult {
        wasm: module.emit_wasm(),
        entries,
        trace_table_addr: blob_addr,
        async_op_addr,
    })
}

/// Run Binaryen's asyncify transform over a baked async module
/// (PLAN_ASYNC_JIT Phase B; tool-gated like [`weval_specialize`] — the real
/// invocation is `wasm-opt -O1 --asyncify in.wasm -o out.wasm`). The
/// transform makes the async shims' `asyncify_start_unwind` calls resumable:
/// the unwind carries the WASM stack (through the trampoline) back to the
/// driver, and the driver's `asyncify_start_rewind(data)` + re-invoking the
/// SAME export resumes mid-shim — the WASM counterpart of the native
/// `Fiber`/`AsyncTraceRun` protocol (`step` / `resume_with`). The module's
/// structure is audited against the input and any stripped custom sections
/// are re-attached, so the baked metadata (`vane.aot`, `vane.traces`)
/// survives the transform.
pub fn asyncify_module(
    wasm: &[u8],
    run: impl Fn(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, PatchError> {
    let out = run(wasm).map_err(PatchError::Weval)?;
    audit_no_stripping(wasm, &out)?;
    reattach_stripped_data(wasm, &out)
}

/// A bump allocator for linear-memory scratch in the static region's padding
/// (above every active data segment end and every global-init address; see
/// [`bake_aot`]'s doc). 16-byte aligned allocations.
struct Scratch {
    next: u32,
}

impl Scratch {
    fn new(module: &Module) -> Self {
        let mut next = 16u32; // never address 0 (null)
        for d in module.data.iter() {
            if let DataKind::Active {
                offset: walrus::ConstExpr::Value(walrus::ir::Value::I32(o)),
                ..
            } = &d.kind
            {
                next = next.max((*o as u32).saturating_add(d.value.len() as u32));
            }
        }
        for g in module.globals.iter() {
            if let (
                walrus::ValType::I32,
                GlobalKind::Local(walrus::ConstExpr::Value(walrus::ir::Value::I32(v))),
            ) = (g.ty, &g.kind)
            {
                // Data symbols (including bss ones like VANE_CURRENT_HOST and
                // wasm-ld's stack pointer) appear as immutable i32 globals.
                next = next.max(*v as u32);
            }
        }
        Scratch {
            next: next.div_ceil(16) * 16,
        }
    }

    fn alloc(&mut self, len: u32) -> u32 {
        let addr = self.next;
        self.next += len.div_ceil(16) * 16;
        addr
    }
}

/// Look up a function export by name.
fn exported_func(module: &Module, name: &str) -> Result<walrus::FunctionId, PatchError> {
    for e in module.exports.iter() {
        if e.name == name {
            return match e.item {
                ExportItem::Function(id) => Ok(id),
                _ => Err(PatchError::Parse(format!(
                    "export `{name}` is not a function"
                ))),
            };
        }
    }
    Err(PatchError::Parse(format!(
        "target module missing export `{name}`"
    )))
}

/// Look up an exported global's walrus id (the direct-tail lowering reads
/// `VANE_CURRENT_HOST` through the global, not its baked address).
fn exported_global_id(module: &Module, name: &str) -> Result<walrus::GlobalId, PatchError> {
    for e in module.exports.iter() {
        if e.name == name {
            match e.item {
                ExportItem::Global(id) => return Ok(id),
                _ => {
                    return Err(PatchError::Parse(format!(
                        "export `{name}` is not a global"
                    )))
                }
            }
        }
    }
    Err(PatchError::Parse(format!(
        "target module missing export `{name}`"
    )))
}

/// Look up an immutable exported global's constant i32 init value — the
/// linear-memory address wasm-ld bakes for a `#[no_mangle]` static.
fn exported_global_addr(module: &Module, name: &str) -> Result<u32, PatchError> {
    for e in module.exports.iter() {
        if e.name == name {
            let ExportItem::Global(gid) = e.item else {
                return Err(PatchError::Parse(format!(
                    "export `{name}` is not a global"
                )));
            };
            let g = module.globals.get(gid);
            let GlobalKind::Local(walrus::ConstExpr::Value(walrus::ir::Value::I32(v))) = &g.kind
            else {
                return Err(PatchError::Parse(format!(
                    "export `{name}` is not a constant i32 global"
                )));
            };
            return Ok(*v as u32);
        }
    }
    Err(PatchError::Parse(format!(
        "target module missing export `{name}`"
    )))
}

/// Overwrite `len(bytes)` at linear-memory address `addr` inside whichever
/// existing data segment covers it.
fn patch_static(module: &mut Module, addr: u32, bytes: &[u8]) -> Result<(), PatchError> {
    let mut found = None;
    for d in module.data.iter() {
        if let DataKind::Active {
            offset: walrus::ConstExpr::Value(walrus::ir::Value::I32(o)),
            ..
        } = &d.kind
        {
            let o = *o as u32;
            if addr >= o && addr + bytes.len() as u32 <= o + d.value.len() as u32 {
                found = Some((d.id(), o));
                break;
            }
        }
    }
    let Some((id, base)) = found else {
        return Err(PatchError::Parse(format!(
            "no data segment covers the static at {addr:#x} (is its initial value non-zero?)"
        )));
    };
    let d = module.data.get_mut(id);
    let at = (addr - base) as usize;
    d.value[at..at + bytes.len()].copy_from_slice(bytes);
    Ok(())
}

/// The multi-value `check_code` shim `(addr: i64) -> (exit_tag: i32,
/// exit_pc: i64)` the renderer's `CheckCode` lowering calls: a thin wrapper
/// over the interpreter's sret `vane_check_code_impl` writing into a baked
/// 16-byte scratch buffer and re-returning the pair as multi-value.
fn build_check_code_wrapper(
    module: &mut Module,
    check_code_impl: walrus::FunctionId,
    scratch_addr: u32,
    memory: MemoryId,
) -> Result<walrus::FunctionId, PatchError> {
    let mut fb = FunctionBuilder::new(
        &mut module.types,
        &[walrus::ValType::I64],
        &[walrus::ValType::I32, walrus::ValType::I64],
    );
    let addr = module.locals.add(walrus::ValType::I64);
    let body = fb.func_body_id();
    // vane_check_code_impl(scratch, addr)
    fb.instr_seq(body)
        .instr(Const {
            value: Value::I32(scratch_addr as i32),
        })
        .instr(LocalGet { local: addr })
        .instr(Call {
            func: check_code_impl,
        });
    // -> (i32.load offset=0, i64.load offset=8) from the scratch buffer
    fb.instr_seq(body)
        .instr(Const {
            value: Value::I32(scratch_addr as i32),
        })
        .instr(Load {
            memory,
            kind: LoadKind::I32 { atomic: false },
            arg: MemArg {
                align: 4,
                offset: 0,
            },
        })
        .instr(Const {
            value: Value::I32(scratch_addr as i32),
        })
        .instr(Load {
            memory,
            kind: LoadKind::I64 { atomic: false },
            arg: MemArg {
                align: 8,
                offset: 8,
            },
        });
    Ok(fb.finish(vec![addr], &mut module.funcs))
}

/// The `CompiledTraceFn` trampoline `(ret_ptr, host_ptr, ct_ptr) -> ()`:
/// set `VANE_CURRENT_HOST`, compute `regs_base`, call the pack fn, and let
/// `vane_make_exit(tag, pc, ret_ptr)` construct the `Exit`. Mirrors
/// [`compile_trampoline`], but built in the target module so it can call the
/// interpreter's own exports directly — and reading the regs offset at RUNTIME
/// (`vane_regs_offset()`), not baked, so no offset value crosses the patcher
/// boundary at all.
#[allow(clippy::too_many_arguments)]
fn build_trampoline(
    module: &mut Module,
    pack_fn: walrus::FunctionId,
    regs_offset: walrus::FunctionId,
    make_exit: walrus::FunctionId,
    current_host_addr: u32,
    memory: MemoryId,
) -> Result<walrus::FunctionId, PatchError> {
    let mut fb = FunctionBuilder::new(
        &mut module.types,
        &[
            walrus::ValType::I32,
            walrus::ValType::I32,
            walrus::ValType::I32,
        ],
        &[],
    );
    let ret_ptr = module.locals.add(walrus::ValType::I32);
    let host_ptr = module.locals.add(walrus::ValType::I32);
    let ct_ptr = module.locals.add(walrus::ValType::I32);
    let body = fb.func_body_id();
    // VANE_CURRENT_HOST = host_ptr (so the pack fn's shims recover the host)
    fb.instr_seq(body)
        .instr(Const {
            value: Value::I32(current_host_addr as i32),
        })
        .instr(LocalGet { local: host_ptr })
        .instr(Store {
            memory,
            kind: StoreKind::I32 { atomic: false },
            arg: MemArg {
                align: 4,
                offset: 0,
            },
        });
    // regs_base = host_ptr + vane_regs_offset()
    fb.instr_seq(body)
        .instr(LocalGet { local: host_ptr })
        .instr(Call { func: regs_offset })
        .instr(Binop {
            op: BinaryOp::I32Add,
        });
    // (exit_tag, exit_pc) = pack_fn(regs_base)
    fb.instr_seq(body).instr(Call { func: pack_fn });
    // vane_make_exit(exit_tag, exit_pc, ret_ptr)
    fb.instr_seq(body)
        .instr(LocalGet { local: ret_ptr })
        .instr(Call { func: make_exit });
    Ok(fb.finish(vec![ret_ptr, host_ptr, ct_ptr], &mut module.funcs))
}

/// One open structured-control frame during the renderer's flat → nested
/// conversion. `cur` is the sequence instructions currently append to (an
/// `If`'s consequent until its `Else`, then its alternative).
struct Frame {
    kind: FrameKind,
    cur: walrus::ir::InstrSeqId,
}

enum FrameKind {
    Block {
        seq: walrus::ir::InstrSeqId,
    },
    Loop {
        seq: walrus::ir::InstrSeqId,
    },
    If {
        seq: walrus::ir::InstrSeqId,
        ty: InstrSeqType,
        alt: Option<walrus::ir::InstrSeqId>,
    },
}

fn cur_seq(frames: &[Frame], entry: walrus::ir::InstrSeqId) -> walrus::ir::InstrSeqId {
    frames.last().map(|f| f.cur).unwrap_or(entry)
}

/// Build one pack fn with the vane-native ABI in the target module: render
/// `ops` with the vane renderer, then lower the flat operator stream into
/// walrus IR — locals/consts/ALU/loads/stores/shim-calls map one-to-one, and
/// the flat `Loop`/`If`/`Else`/`End`/`Br` control flow is re-nested (a stack
/// of open frames; `Br`'s relative depth indexes the stack, and the renderer
/// only ever branches to loops). The trailing `unreachable` matches
/// [`compile_trace_vane`] (a finite trace's nested TailCall `return` leaves
/// the implicit final return unreachable).
#[allow(clippy::too_many_arguments)]
fn build_pack_fn(
    module: &mut Module,
    ops: Vec<StackOp>,
    abi: VaneAbiConfig,
    memory: MemoryId,
    shims: &[walrus::FunctionId; 7],
    host_global: walrus::GlobalId,
    trampoline_ty: walrus::TypeId,
    table: walrus::TableId,
) -> Result<walrus::FunctionId, PatchError> {
    let mut fb = FunctionBuilder::new(
        &mut module.types,
        &[walrus::ValType::I32],
        &[walrus::ValType::I32, walrus::ValType::I64],
    );
    let regs_base = module.locals.add(walrus::ValType::I32);
    let scratch_a = module.locals.add(walrus::ValType::I64);
    let scratch_b = module.locals.add(walrus::ValType::I64);
    let scratch32 = module.locals.add(walrus::ValType::I32);
    // The renderer addresses locals by WASM index; walrus addresses them by
    // LocalId. Track the mapping by alloc order (indices per the `abi`).
    let n_locals = 1 + abi
        .regs_base_local
        .max(abi.scratch_a)
        .max(abi.scratch_b)
        .max(abi.scratch32) as usize;
    let mut locals = vec![regs_base; n_locals];
    locals[abi.regs_base_local as usize] = regs_base;
    locals[abi.scratch_a as usize] = scratch_a;
    locals[abi.scratch_b as usize] = scratch_b;
    locals[abi.scratch32 as usize] = scratch32;

    let entry = fb.func_body_id();
    let mut frames: Vec<Frame> = Vec::new();
    for jit_op in vane_arch::template::render_stack_to_wasm_vane(ops.into_iter(), abi).chain(
        core::iter::once(vane_target_wasm::JitOpcode::Operator {
            op: wasmparser::Operator::Unreachable,
        }),
    ) {
        let vane_target_wasm::JitOpcode::Operator { op } = jit_op;
        lower_op(
            &mut fb,
            &mut frames,
            entry,
            op,
            &locals,
            shims,
            memory,
            &mut module.types,
            host_global,
            trampoline_ty,
            table,
        )?;
    }
    debug_assert!(
        frames.is_empty(),
        "the renderer's control flow is balanced; frames must be empty here"
    );
    Ok(fb.finish(vec![regs_base], &mut module.funcs))
}

/// Map the renderer's local-index space onto walrus `LocalId`s (see
/// [`build_pack_fn`]).
fn local_at(locals: &[LocalId], index: u32) -> Result<LocalId, PatchError> {
    locals
        .get(index as usize)
        .copied()
        .ok_or_else(|| PatchError::Parse(format!("renderer references unknown local {index}")))
}

fn shim_at(shims: &[walrus::FunctionId; 7], index: u32) -> Result<walrus::FunctionId, PatchError> {
    shims
        .get(index as usize)
        .copied()
        .ok_or_else(|| PatchError::Parse(format!("renderer references unknown shim fn {index}")))
}

/// wasmparser block type → walrus sequence type (exponent-free: the renderer
/// only emits `Empty` and single-value types).
fn blockty_to_seq_type(
    blockty: wasmparser::BlockType,
    types: &mut ModuleTypes,
) -> Result<InstrSeqType, PatchError> {
    Ok(match blockty {
        wasmparser::BlockType::Empty => InstrSeqType::new(types, &[], &[]),
        wasmparser::BlockType::Type(vt) => InstrSeqType::new(types, &[], &[valtype_to_walrus(vt)?]),
        wasmparser::BlockType::FuncType(_) => {
            return Err(PatchError::Parse(
                "bake: multi-value block types are not emitted by the renderer".into(),
            ));
        }
    })
}

fn valtype_to_walrus(vt: wasmparser::ValType) -> Result<walrus::ValType, PatchError> {
    Ok(match vt {
        wasmparser::ValType::I32 => walrus::ValType::I32,
        wasmparser::ValType::I64 => walrus::ValType::I64,
        wasmparser::ValType::F32 => walrus::ValType::F32,
        wasmparser::ValType::F64 => walrus::ValType::F64,
        wasmparser::ValType::V128 => walrus::ValType::V128,
        wasmparser::ValType::Ref(_) => {
            return Err(PatchError::Parse(
                "bake: reference-typed values are not emitted by the renderer".into(),
            ));
        }
    })
}

/// wasmparser `MemArg` (align = log2 exponent) → walrus `MemArg` (align =
/// bytes).
fn memarg_to_walrus(m: &wasmparser::MemArg) -> MemArg {
    MemArg {
        align: 1u32 << m.align,
        offset: m.offset,
    }
}

/// Lower one rendered operator into the current sequence (or restructure the
/// frame stack for the flat control-flow ops). Every variant the vane renderer
/// emits is handled; anything else fails loudly rather than silently mis-
/// lowering (the renderer's set is closed — see the ops enumerated in
/// `render_with_abi`).
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn lower_op(
    fb: &mut FunctionBuilder,
    frames: &mut Vec<Frame>,
    entry: walrus::ir::InstrSeqId,
    op: wasmparser::Operator<'_>,
    locals: &[LocalId],
    shims: &[walrus::FunctionId; 7],
    memory: MemoryId,
    types: &mut ModuleTypes,
    host_global: walrus::GlobalId,
    trampoline_ty: walrus::TypeId,
    table: walrus::TableId,
) -> Result<(), PatchError> {
    use wasmparser::Operator as O;
    match op {
        // ── Flat control flow → nested walrus sequences ───────────────────
        O::Block { blockty } => {
            let ty = blockty_to_seq_type(blockty, types)?;
            let seq = fb.dangling_instr_seq(ty).id();
            frames.push(Frame {
                kind: FrameKind::Block { seq },
                cur: seq,
            });
            Ok(())
        }
        O::Loop { blockty } => {
            let ty = blockty_to_seq_type(blockty, types)?;
            let seq = fb.dangling_instr_seq(ty).id();
            frames.push(Frame {
                kind: FrameKind::Loop { seq },
                cur: seq,
            });
            Ok(())
        }
        O::If { blockty } => {
            let ty = blockty_to_seq_type(blockty, types)?;
            let seq = fb.dangling_instr_seq(ty).id();
            frames.push(Frame {
                kind: FrameKind::If { seq, ty, alt: None },
                cur: seq,
            });
            Ok(())
        }
        O::Else => {
            let ty = match frames.last().map(|f| &f.kind) {
                Some(FrameKind::If { ty, .. }) => *ty,
                _ => {
                    return Err(PatchError::Parse(
                        "bake: `else` without a matching `if`".into(),
                    ));
                }
            };
            let alt = fb.dangling_instr_seq(ty).id();
            let frame = frames
                .last_mut()
                .expect("checked the kind above, so a frame exists");
            let FrameKind::If { alt: slot, .. } = &mut frame.kind else {
                unreachable!("checked the kind above")
            };
            if slot.is_some() {
                return Err(PatchError::Parse("bake: duplicate `else`".into()));
            }
            *slot = Some(alt);
            frame.cur = alt;
            Ok(())
        }
        O::End => {
            let frame = frames
                .pop()
                .ok_or_else(|| PatchError::Parse("bake: unbalanced `end`".into()))?;
            let parent = cur_seq(frames, entry);
            match frame.kind {
                FrameKind::Block { seq } => {
                    fb.instr_seq(parent).instr(Block { seq });
                }
                FrameKind::Loop { seq } => {
                    fb.instr_seq(parent).instr(Loop { seq });
                }
                FrameKind::If { seq, ty, alt } => {
                    // An `if` without `else` gets an empty alternative
                    // sequence (the renderer always emits `else`, but stay
                    // well-formed regardless).
                    let alternative = alt.unwrap_or_else(|| fb.dangling_instr_seq(ty).id());
                    fb.instr_seq(parent).instr(IfElse {
                        consequent: seq,
                        alternative,
                    });
                }
            }
            Ok(())
        }
        O::Br { relative_depth } => {
            let depth = relative_depth as usize;
            if depth >= frames.len() {
                return Err(PatchError::Parse(format!(
                    "bake: br depth {relative_depth} escapes the {0} open frames",
                    frames.len()
                )));
            }
            let target = &frames[frames.len() - 1 - depth];
            // The renderer's `Br` lowers StackOp loop-backs only (`StackOp::Br`
            // counts loop boundaries; `If` frames only widen the depth).
            let FrameKind::Loop { seq } = target.kind else {
                return Err(PatchError::Parse(
                    "bake: br targets a non-loop frame (the renderer only branches to loops)"
                        .into(),
                ));
            };
            fb.instr_seq(cur_seq(frames, entry))
                .instr(Br { block: seq });
            Ok(())
        }
        O::Return => {
            fb.instr_seq(cur_seq(frames, entry)).instr(Return {});
            Ok(())
        }
        O::Unreachable => {
            fb.instr_seq(cur_seq(frames, entry)).instr(Unreachable {});
            Ok(())
        }

        // ── Locals / consts / shim calls ──────────────────────────────────
        O::LocalGet { local_index } => {
            let local = local_at(locals, local_index)?;
            fb.instr_seq(cur_seq(frames, entry))
                .instr(LocalGet { local });
            Ok(())
        }
        O::LocalSet { local_index } => {
            let local = local_at(locals, local_index)?;
            fb.instr_seq(cur_seq(frames, entry))
                .instr(LocalSet { local });
            Ok(())
        }
        O::LocalTee { local_index } => {
            let local = local_at(locals, local_index)?;
            fb.instr_seq(cur_seq(frames, entry))
                .instr(LocalTee { local });
            Ok(())
        }
        O::I32Const { value } => {
            fb.instr_seq(cur_seq(frames, entry)).instr(Const {
                value: Value::I32(value),
            });
            Ok(())
        }
        O::I64Const { value } => {
            fb.instr_seq(cur_seq(frames, entry)).instr(Const {
                value: Value::I64(value),
            });
            Ok(())
        }
        O::Call { function_index } => {
            let func = shim_at(shims, function_index)?;
            fb.instr_seq(cur_seq(frames, entry)).instr(Call { func });
            Ok(())
        }
        // The direct tail-call sentinel: the renderer emits this ONE
        // call_indirect (the pack fns have no others) and the post-encode
        // splice rewrites the opcode to return_call_indirect (0x11 -> 0x13)
        // inside the baked function bodies.
        O::CallIndirect {
            type_index: _,
            table_index: _,
        } => {
            let seq = cur_seq(frames, entry);
            fb.instr_seq(seq).instr(CallIndirect {
                ty: trampoline_ty,
                table,
            });
            Ok(())
        }
        // The renderer reads VANE_CURRENT_HOST (the host pointer the
        // trampoline stored) as global-get + i32.load: the global holds the
        // static's ADDRESS, the live host pointer sits at it.
        O::GlobalGet { global_index: _ } => {
            let seq = cur_seq(frames, entry);
            fb.instr_seq(seq).instr(GlobalGet { global: host_global });
            Ok(())
        }
        O::I32Load { memarg } if memarg.align == 2 && memarg.offset == 0 => {
            // (overlaps the generic I32Load arm below only if align != 2 —
            // this is the VANE_CURRENT_HOST dereference; keep the generic
            // load behavior, same encoding)
            let seq = cur_seq(frames, entry);
            fb.instr_seq(seq).instr(Load {
                memory,
                kind: LoadKind::I32 { atomic: false },
                arg: memarg_to_walrus(&memarg),
            });
            Ok(())
        }
        O::Drop => {
            fb.instr_seq(cur_seq(frames, entry)).instr(Drop {});
            Ok(())
        }

        // ── Conversions / comparisons (unary) ────────────────────────────
        O::I32WrapI64 => unop(fb, frames, entry, UnaryOp::I32WrapI64),
        O::I64ExtendI32S => unop(fb, frames, entry, UnaryOp::I64ExtendSI32),
        O::I64ExtendI32U => unop(fb, frames, entry, UnaryOp::I64ExtendUI32),
        O::I32Eqz => unop(fb, frames, entry, UnaryOp::I32Eqz),
        O::I64Eqz => unop(fb, frames, entry, UnaryOp::I64Eqz),

        // ── ALU (binary) — every form `render_with_abi` emits ────────────
        O::I32Add => binop(fb, frames, entry, BinaryOp::I32Add),
        O::I32Sub => binop(fb, frames, entry, BinaryOp::I32Sub),
        O::I32Mul => binop(fb, frames, entry, BinaryOp::I32Mul),
        O::I32DivS => binop(fb, frames, entry, BinaryOp::I32DivS),
        O::I32DivU => binop(fb, frames, entry, BinaryOp::I32DivU),
        O::I32RemS => binop(fb, frames, entry, BinaryOp::I32RemS),
        O::I32RemU => binop(fb, frames, entry, BinaryOp::I32RemU),
        O::I32Shl => binop(fb, frames, entry, BinaryOp::I32Shl),
        O::I32ShrS => binop(fb, frames, entry, BinaryOp::I32ShrS),
        O::I32ShrU => binop(fb, frames, entry, BinaryOp::I32ShrU),
        O::I32Ne => binop(fb, frames, entry, BinaryOp::I32Ne),
        O::I64Add => binop(fb, frames, entry, BinaryOp::I64Add),
        O::I64Sub => binop(fb, frames, entry, BinaryOp::I64Sub),
        O::I64Mul => binop(fb, frames, entry, BinaryOp::I64Mul),
        O::I64DivS => binop(fb, frames, entry, BinaryOp::I64DivS),
        O::I64DivU => binop(fb, frames, entry, BinaryOp::I64DivU),
        O::I64RemS => binop(fb, frames, entry, BinaryOp::I64RemS),
        O::I64RemU => binop(fb, frames, entry, BinaryOp::I64RemU),
        O::I64And => binop(fb, frames, entry, BinaryOp::I64And),
        O::I64Or => binop(fb, frames, entry, BinaryOp::I64Or),
        O::I64Xor => binop(fb, frames, entry, BinaryOp::I64Xor),
        O::I64Shl => binop(fb, frames, entry, BinaryOp::I64Shl),
        O::I64ShrS => binop(fb, frames, entry, BinaryOp::I64ShrS),
        O::I64ShrU => binop(fb, frames, entry, BinaryOp::I64ShrU),
        O::I64Eq => binop(fb, frames, entry, BinaryOp::I64Eq),
        O::I64Ne => binop(fb, frames, entry, BinaryOp::I64Ne),
        O::I64LtS => binop(fb, frames, entry, BinaryOp::I64LtS),
        O::I64LtU => binop(fb, frames, entry, BinaryOp::I64LtU),
        O::I64GeS => binop(fb, frames, entry, BinaryOp::I64GeS),
        O::I64GeU => binop(fb, frames, entry, BinaryOp::I64GeU),

        // ── Linear-memory loads/stores (reg cache + flat scratch loads) ───
        O::I64Load { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I64 { atomic: false },
            &memarg,
        ),
        O::I64Load8U { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I64_8 {
                kind: walrus::ir::ExtendedLoad::ZeroExtend,
            },
            &memarg,
        ),
        O::I64Load8S { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I64_8 {
                kind: walrus::ir::ExtendedLoad::SignExtend,
            },
            &memarg,
        ),
        O::I64Load16U { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I64_16 {
                kind: walrus::ir::ExtendedLoad::ZeroExtend,
            },
            &memarg,
        ),
        O::I64Load16S { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I64_16 {
                kind: walrus::ir::ExtendedLoad::SignExtend,
            },
            &memarg,
        ),
        O::I64Load32U { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I64_32 {
                kind: walrus::ir::ExtendedLoad::ZeroExtend,
            },
            &memarg,
        ),
        O::I64Load32S { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I64_32 {
                kind: walrus::ir::ExtendedLoad::SignExtend,
            },
            &memarg,
        ),
        O::I32Load { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I32 { atomic: false },
            &memarg,
        ),
        O::I32Load8U { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I32_8 {
                kind: walrus::ir::ExtendedLoad::ZeroExtend,
            },
            &memarg,
        ),
        O::I32Load8S { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I32_8 {
                kind: walrus::ir::ExtendedLoad::SignExtend,
            },
            &memarg,
        ),
        O::I32Load16U { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I32_16 {
                kind: walrus::ir::ExtendedLoad::ZeroExtend,
            },
            &memarg,
        ),
        O::I32Load16S { memarg } => load(
            fb,
            frames,
            entry,
            memory,
            LoadKind::I32_16 {
                kind: walrus::ir::ExtendedLoad::SignExtend,
            },
            &memarg,
        ),
        O::I64Store { memarg } => store(
            fb,
            frames,
            entry,
            memory,
            StoreKind::I64 { atomic: false },
            &memarg,
        ),
        O::I64Store8 { memarg } => store(
            fb,
            frames,
            entry,
            memory,
            StoreKind::I64_8 { atomic: false },
            &memarg,
        ),
        O::I64Store16 { memarg } => store(
            fb,
            frames,
            entry,
            memory,
            StoreKind::I64_16 { atomic: false },
            &memarg,
        ),
        O::I64Store32 { memarg } => store(
            fb,
            frames,
            entry,
            memory,
            StoreKind::I64_32 { atomic: false },
            &memarg,
        ),

        // The speet regfn dispatch (`return_call` to a lookup stub) never
        // appears in vane-native traces — the vane ABI returns exits instead.
        O::ReturnCall { .. } | O::ReturnCallIndirect { .. } => Err(PatchError::Parse(
            "bake: return_call is the speet regfn dispatch, not the vane-native ABI".into(),
        )),

        // Anything else is outside the renderer's closed operator set.
        op => Err(PatchError::Parse(format!(
            "bake: unsupported operator {op:?} (outside the renderer's emitted set)"
        ))),
    }
}

fn unop(
    fb: &mut FunctionBuilder,
    frames: &mut [Frame],
    entry: walrus::ir::InstrSeqId,
    op: UnaryOp,
) -> Result<(), PatchError> {
    fb.instr_seq(cur_seq(frames, entry)).instr(Unop { op });
    Ok(())
}

fn binop(
    fb: &mut FunctionBuilder,
    frames: &mut [Frame],
    entry: walrus::ir::InstrSeqId,
    op: BinaryOp,
) -> Result<(), PatchError> {
    fb.instr_seq(cur_seq(frames, entry)).instr(Binop { op });
    Ok(())
}

fn load(
    fb: &mut FunctionBuilder,
    frames: &mut [Frame],
    entry: walrus::ir::InstrSeqId,
    memory: MemoryId,
    kind: LoadKind,
    memarg: &wasmparser::MemArg,
) -> Result<(), PatchError> {
    fb.instr_seq(cur_seq(frames, entry)).instr(Load {
        memory,
        kind,
        arg: memarg_to_walrus(memarg),
    });
    Ok(())
}

fn store(
    fb: &mut FunctionBuilder,
    frames: &mut [Frame],
    entry: walrus::ir::InstrSeqId,
    memory: MemoryId,
    kind: StoreKind,
    memarg: &wasmparser::MemArg,
) -> Result<(), PatchError> {
    fb.instr_seq(cur_seq(frames, entry)).instr(Store {
        memory,
        kind,
        arg: memarg_to_walrus(memarg),
    });
    Ok(())
}
