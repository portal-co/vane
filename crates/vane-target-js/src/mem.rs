//! Memory code-generation helpers for the vane JavaScript target.
//!
//! This wires `os-page-codegen`’s generic `JsBackend` into vane's JS
//! renderer, producing the legacy `data(addr)` function and per-access
//! load/store snippets that match the existing vane output style.

use alloc::string::String;
use core::fmt::Write as FmtWrite;

use os_build::{MemoryAccessOp, MemoryCodegen};
use os_page::MemorySpec;
use os_page_codegen::JsBackend;
use os_target_core::{Backend as _, OsOp};

/// A vane-flavoured memory code-generator for JavaScript.
///
/// It currently supports the legacy/identity memory model (one page lookup,
/// no shared page table walk). Shared paging is emitted as a placeholder
/// marker so generated code fails fast until the full walk is implemented.
pub struct CoreJsMemoryCodegen;

impl Default for CoreJsMemoryCodegen {
    fn default() -> Self {
        Self
    }
}

impl CoreJsMemoryCodegen {
    /// Emit a top-level `data(v)` helper matching vane's legacy JS idiom.
    ///
    /// The helper returns a `DataView` rooted at the page containing `v`:
    ///
    /// ```js
    /// data=(v=>{let p=$.get_page(Number(v));return new DataView($._sys('memory').buffer,p);})
    /// ```
    ///
    /// For non-legacy / shared specs the output contains a `throw` so the
    /// mismatch is obvious at run time.
    pub fn emit_data_function(out: &mut String, spec: &MemorySpec) {
        if is_legacy(spec) {
            let _ = writeln!(
                out,
                "data=(v=>{{let p=$.get_page(Number(v));return new DataView($._sys('memory').buffer,p);}})"
            );
        } else {
            let _ = writeln!(
                out,
                "data=(v=>{{throw new Error('shared paging not yet generated for this spec');}})"
            );
        }
    }
}

fn is_legacy(spec: &MemorySpec) -> bool {
    // Legacy identity mapping uses 64 KiB pages with a single level and
    // 64-bit physical addresses. Anything else is treated as shared/paging.
    spec.page_size_log2 == MemorySpec::WASM_64K.page_size_log2
        && spec.levels == MemorySpec::WASM_64K.levels
        && spec.physical_address_bits >= MemorySpec::WASM_64K.physical_address_bits
}

impl<'a> MemoryCodegen<JsBackend<'a>> for CoreJsMemoryCodegen {
    fn emit_memory_access(&mut self, backend: &mut JsBackend<'a>, op: &MemoryAccessOp) {
        if op.write {
            backend.op(OsOp::Store { width: op.width });
        } else {
            backend.op(OsOp::Load {
                width: op.width,
                signed: op.signed,
            });
        }
    }

    fn emit_page_table_glue(&mut self, backend: &mut JsBackend<'a>, spec: &MemorySpec) {
        Self::emit_data_function(backend.out, spec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use os_target_core::MemWidth;

    #[test]
    fn emits_legacy_data_function() {
        let mut out = String::new();
        let mut b = JsBackend::new(&mut out);
        CoreJsMemoryCodegen.emit_page_table_glue(&mut b, &MemorySpec::WASM_64K);
        let text = b.out.as_str();
        assert!(text.contains("data=(v=>"));
        assert!(text.contains("$.get_page(Number(v))"));
        assert!(text.contains("new DataView($._sys('memory').buffer"));
    }

    #[test]
    fn shared_spec_emits_failing_stub() {
        let mut out = String::new();
        let mut b = JsBackend::new(&mut out);
        let spec = MemorySpec {
            page_size_log2: 12,
            levels: 2,
            physical_address_bits: 56,
        };
        CoreJsMemoryCodegen.emit_page_table_glue(&mut b, &spec);
        assert!(b.out.contains("shared paging not yet generated"));
    }

    #[test]
    fn load_uses_data_helper() {
        let mut out = String::new();
        let mut b = JsBackend::new(&mut out);
        CoreJsMemoryCodegen.emit_memory_access(
            &mut b,
            &MemoryAccessOp {
                width: MemWidth::W32,
                write: false,
                signed: true,
            },
        );
        assert!(b.out.contains("get_page"));
        assert!(b.out.contains("getInt32"));
    }

    #[test]
    fn store_uses_data_helper() {
        let mut out = String::new();
        let mut b = JsBackend::new(&mut out);
        CoreJsMemoryCodegen.emit_memory_access(
            &mut b,
            &MemoryAccessOp {
                width: MemWidth::W16,
                write: true,
                signed: false,
            },
        );
        assert!(b.out.contains("get_page"));
        assert!(b.out.contains("setUint16"));
    }
}