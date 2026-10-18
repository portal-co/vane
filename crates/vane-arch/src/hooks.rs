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

/// How to capture a string argument at a hooked symbol: read the pointer
/// in argument register a<reg>, add `offset` bytes, follow that pointer,
/// and read a NUL-terminated string there. Covers the real kernel shapes —
/// e.g. `do_execveat_common`'s `struct filename *` (name at offset 0) —
/// without baking struct layouts into either engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgStr {
    /// Argument register index (0 = a0).
    pub reg: u8,
    /// Byte offset from the register's pointer to the string pointer.
    pub offset: u64,
}

/// One hook: a named observation point at a guest virtual address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    /// Symbol name at the real location (e.g. `do_execveat_common`).
    pub name: String,
    /// Guest virtual address of the hook (the symbol's st_value).
    pub addr: u64,
    /// Optional string capture rule (manifest `str:aN:off`).
    pub arg_str: Option<ArgStr>,
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
        self.by_addr.insert(
            addr,
            Hook {
                name: name.to_owned(),
                addr,
                arg_str: None,
            },
        );
    }

    /// Install a hook with a string-capture rule.
    pub fn insert_with_str(&mut self, addr: u64, name: &str, arg_str: ArgStr) {
        self.by_addr.insert(
            addr,
            Hook {
                name: name.to_owned(),
                addr,
                arg_str: Some(arg_str),
            },
        );
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
    ///
    /// ```
    /// let set = vane_arch::hooks::HookSet::parse_manifest(
    ///     "# comment\n0xffffffff80000100 do_execveat_common\n\n0xffffffff80000200 do_sys_openat2\n",
    /// )
    /// .unwrap();
    /// assert_eq!(set.len(), 2);
    /// assert_eq!(set.at(0xffffffff80000100).unwrap().name, "do_execveat_common");
    /// assert!(vane_arch::hooks::HookSet::parse_manifest("not-an-addr").is_err());
    /// ```
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
    ///
    /// ```
    /// let map = "ffffffff80000100 T do_execveat_common\nffffffff80000200 T do_sys_openat2\n";
    /// let set = vane_arch::hooks::HookSet::from_system_map(
    ///     map,
    ///     &["do_execveat_common", "do_sys_openat2"],
    /// )
    /// .unwrap();
    /// assert!(set.is_boundary(0xffffffff80000100));
    /// assert!(!set.is_boundary(0xffffffff80000104));
    /// assert!(vane_arch::hooks::HookSet::from_system_map(map, &["no_such_symbol"]).is_err());
    /// ```
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
        if missing.is_empty() {
            Ok(set)
        } else {
            Err(missing)
        }
    }
}
