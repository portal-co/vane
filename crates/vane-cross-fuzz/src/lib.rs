//! `vane-cross-fuzz` — differential test harness executing Vane's
//! architecture frontends (RV64, AArch64) against Unicorn 2.x on random
//! instruction streams.
//!
//! **Host-only:** links the native Unicorn C library; never compiled for
//! `wasm32`. See `CROSS_FUZZ_PLAN.md` for the full design.
//!
//! # Layout
//!
//! - [`skip`] — which Vane traps mean "skip" vs "fail" (single source of truth).
//! - [`state`] — `StackHost` over `Mem` + a snapshot-able register image.
//! - [`compare`] — field-by-field state diffing and failure reports.
//! - [`runner`] — the step-differential engine owning Unicorn + `Mem`.
//! - [`rv64`] / [`aarch64`] — per-frontend glue (trace emission + profiles).
//!
//! Feature-gated: `rv64` enables the RISC-V target, `aarch64` the AArch64
//! target (both pull the matching frontend crate's `fuzz-traces` feature).

#![no_std]
extern crate alloc;

pub mod compare;
pub mod runner;
pub mod skip;
pub mod state;

// `interp` is re-exported through vane-arch's public API; alias the path the
// runner uses so the reference compiles regardless of frontend features.
pub use vane_arch::interp;

#[cfg(feature = "rv64")]
pub mod rv64;

#[cfg(feature = "aarch64")]
pub mod aarch64;
