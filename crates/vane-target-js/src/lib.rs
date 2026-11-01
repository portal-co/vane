#![no_std]
extern crate alloc;

pub mod flate;
pub mod mem;

pub use flate::{DebugFlate, Flate, ReleaseFlate};

/// The JS perf runtime (PageMap / Tlb / PagedRuntime — the JS ports of the
/// native PageMap and direct-mapped TlbCache). Plain-script IIFE; embed with
/// `run_raw_code(PERF_JS)` (the wsdom bootstrap does exactly that) or eval
/// in any JS context; installs `globalThis.__vanePerf`.
pub const PERF_JS: &str = include_str!("../js/perf.js");

#[cfg(test)]
mod tests {
    #[test]
    fn perf_js_embeds_the_structures() {
        assert!(super::PERF_JS.contains("class PageMap"));
        assert!(super::PERF_JS.contains("class Tlb"));
        assert!(super::PERF_JS.contains("class PagedRuntime"));
        assert!(super::PERF_JS.contains("globalThis.__vanePerf"));
    }
}
