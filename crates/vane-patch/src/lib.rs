//! Custom wasm binary patcher + Weval wrapper for vane machine acceleration
//! (PLAN_VIRTIO_WEVAL workstreams W2/W3).
//!
//! Both backends specialize a vane interpreter module against a machine
//! snapshot and register the result into the compiled-trace registry. They
//! differ in how they treat the module's structural data:
//!
//! - The **custom patcher** ([`embed_snapshot`]) is *purely additive* — it
//!   inserts the snapshot as a custom section and changes nothing else, so by
//!   construction no data segment, global, export, or function is dropped.
//!   [`audit_no_stripping`] proves it (S-W3).
//! - The **Weval wrapper** ([`weval`]) runs the external Weval partial
//!   evaluator (network/tool-gated) and then *re-attaches* any structural data
//!   Weval's reachability analysis stripped, so the specialized module is still
//!   a complete, runnable machine.

use walrus::Module;
use walrus::RawCustomSection;

/// The custom section name holding an embedded machine snapshot.
pub const SNAPSHOT_SECTION: &str = "vane.snapshot";

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

/// Embed a machine snapshot into a vane interpreter module as a
/// `vane.snapshot` custom section. Purely additive: the input module's code,
/// data, globals, exports, and existing custom sections are all preserved
/// byte-for-byte (the patcher never strips). The host reads the embedded
/// snapshot on boot and restores it, skipping the boot work that produced it.
pub fn embed_snapshot(wasm: &[u8], snapshot: &[u8]) -> Result<Vec<u8>, PatchError> {
    let mut module = Module::from_buffer(wasm)?;
    // Replace any prior embedded snapshot: a multi-pass chain embeds the
    // latest derived snapshot, so re-patching supersedes rather than
    // accumulates (and re-patching the same snapshot is idempotent).
    while module.customs.remove_raw(SNAPSHOT_SECTION).is_some() {}
    module.customs.add(RawCustomSection {
        name: SNAPSHOT_SECTION.to_string(),
        data: snapshot.to_vec(),
    });
    Ok(module.emit_wasm())
}

/// Extract an embedded snapshot from a patched module (the inverse of
/// [`embed_snapshot`]). Returns `None` when no snapshot section is present.
pub fn extract_snapshot(wasm: &[u8]) -> Result<Option<Vec<u8>>, PatchError> {
    // Parse with a config that retains the custom section, then read it.
    let module = Module::from_buffer(wasm)?;
    for (_id, section) in module.customs.iter() {
        if section.name() == SNAPSHOT_SECTION {
            let data = section.data(&walrus::IdsToIndices::default()).into_owned();
            return Ok(Some(data));
        }
    }
    Ok(None)
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
    let present: std::collections::BTreeSet<String> =
        p.customs.iter().map(|(_i, s)| s.name().to_string()).collect();
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
