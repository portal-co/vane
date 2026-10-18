//! Hook system: observation points at **real guest symbol locations**
//! (spday/plans/2026-10-18-vane-fullvirt-hooks.md work item B).
//!
//! A [`HookSet`] names guest virtual addresses — normally kernel function
//! entries taken from the booted kernel's `System.map` / kallsyms — where
//! execution must be *observed*, not redirected. Two halves cooperate:
//!
//! - **Emission**: the trace emitter receives `Params::boundaries` and cuts
//!   the trace-inlining chain at every hooked address, so a hooked address
//!   always starts its own trace and can never hide mid-trace.
//! - **Dispatch**: the reactor's trace loop records a [`HookEvent`] whenever
//!   a trace starts at a hooked address, capturing the live argument
//!   registers (a0–a7 on rv64) — because hooks sit at real symbol locations,
//!   those registers are the function's real arguments.
//!
//! This replaces observing syscalls via a user-mode ecall ABI (the
//! "micro-OS"): hooks fire on the *kernel's own* entry points with the
//! *kernel's own* addresses, so a guest sees a real Linux throughout.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// One hook: a named observation point at a guest virtual address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    /// Symbol name at the real location (e.g. `do_execveat_common`).
    pub name: String,
    /// Guest virtual address of the hook (the symbol's st_value).
    pub addr: u64,
}

/// A recorded hook firing: arrival at a hooked symbol with the argument
/// registers live at that moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookEvent {
    /// The hook's symbol name.
    pub name: String,
    /// The hooked address (equals `pc`; kept for self-contained records).
    pub addr: u64,
    /// Guest pc at firing (start of the hooked trace).
    pub pc: u64,
    /// Argument registers a0–a7 (rv64 calling convention) at entry.
    pub args: [u64; 8],
}

/// The set of installed hooks, keyed by guest address.
#[derive(Clone, Debug, Default)]
pub struct HookSet {
    by_addr: BTreeMap<u64, Hook>,
}

impl HookSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a hook at `addr` named `name`. Re-installing the same
    /// address replaces the name.
    pub fn insert(&mut self, addr: u64, name: &str) {
        self.by_addr.insert(addr, Hook { name: name.to_owned(), addr });
    }

    /// The hook at `addr`, if any.
    pub fn at(&self, addr: u64) -> Option<&Hook> {
        self.by_addr.get(&addr)
    }

    /// Whether `addr` is a hook boundary (the emission-time predicate).
    pub fn is_boundary(&self, addr: u64) -> bool {
        self.by_addr.contains_key(&addr)
    }

    pub fn is_empty(&self) -> bool {
        self.by_addr.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_addr.len()
    }

    pub fn hooks(&self) -> impl Iterator<Item = &Hook> {
        self.by_addr.values()
    }

    /// Parse a hook manifest: one `<hexaddr> <name>` pair per line (`#`
    /// comments and blank lines ignored). This is the format the QEMU
    /// plugin consumes too, so one manifest drives both engines.
    pub fn parse_manifest(text: &str) -> Result<Self, String> {
        let mut set = HookSet::new();
        for (ln, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(addr), Some(name)) = (parts.next(), parts.next()) else {
                return Err(alloc::format!(
                    "manifest line {}: want `<hexaddr> <name>`",
                    ln + 1
                ));
            };
            let addr = addr.strip_prefix("0x").unwrap_or(addr);
            let addr = u64::from_str_radix(addr, 16)
                .map_err(|e| alloc::format!("manifest line {}: bad address: {e}", ln + 1))?;
            set.insert(addr, name);
        }
        Ok(set)
    }

    /// Build a hook set from a Linux `System.map` (`<hexaddr> <type>
    /// <name>` lines), keeping only the named symbols. Unknown names are
    /// reported so a stale map against a new kernel fails visibly instead
    /// of silently tracing nothing.
    pub fn from_system_map(text: &str, names: &[&str]) -> Result<Self, Vec<String>> {
        let mut set = HookSet::new();
        let mut missing: Vec<String> = names.iter().map(|s| (*s).to_owned()).collect();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let (Some(addr), Some(_ty), Some(name)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if let Some(pos) = missing.iter().position(|n| n == name) {
                let addr = u64::from_str_radix(addr, 16)
                    .map_err(|_| alloc::vec![alloc::format!("bad System.map line: {line}")])?;
                set.insert(addr, name);
                missing.remove(pos);
            }
        }
        if missing.is_empty() { Ok(set) } else { Err(missing) }
    }
}
