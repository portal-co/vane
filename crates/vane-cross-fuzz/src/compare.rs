//! State comparison between Vane's [`MachineState`] and Unicorn's register
//! file. Field-by-field diffing so a failure report names exactly which
//! registers diverged (CROSS_FUZZ_PLAN.md §9).

use alloc::{string::String, vec::Vec};
use core::fmt::Write as _;

use crate::state::MachineState;

/// One diverging architectural field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDiff {
    /// Human-readable field name (e.g. `x3`, `nzcv.c`, `f7`, `pc`).
    pub name: String,
    pub vane: u64,
    pub unicorn: u64,
}

impl FieldDiff {
    fn render(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "  {:<8} vane={:#018x} unicorn={:#018x}",
            self.name, self.vane, self.unicorn
        );
    }
}

/// Compare two machine states field by field.
///
/// `reg_names[i]` names register slot `i`; slot names that only exist in one
/// side's image are skipped via `only_in_vane_ok` (Unicorn exposes registers
/// Vane does not model and vice versa; the caller declares the mapping).
pub fn diff_states(
    vane: &MachineState,
    unicorn: &MachineState,
    reg_names: &[&str; 32],
) -> Vec<FieldDiff> {
    let _ = reg_names; // superseded per-arch naming: see diff_states_named
    diff_states_named(vane, unicorn)
}

/// Compare two machine states field by field with **indexed** register
/// names (`x0`, `x1`, … / `x31` = SP label per profile's `reg_names`).
///
/// The `[&str; 32]`-based variant proved ambiguous (every RV slot rendered
/// as `"x"`, hiding which register diverged), so names are now derived from
/// the slot index directly; `reg_names` is kept in the trait for profile
/// docs and report headers.
pub fn diff_states_named(vane: &MachineState, unicorn: &MachineState) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();
    for i in 0..32 {
        if vane.regs[i] != unicorn.regs[i] {
            diffs.push(FieldDiff {
                name: alloc::format!("x{i}"),
                vane: vane.regs[i],
                unicorn: unicorn.regs[i],
            });
        }
    }
    // Slots present in either image but unequal (or missing on the Unicorn
    // side, where a missing slot reads as 0 via `MachineState::slot`).
    let mut names: Vec<&String> = vane.slots.keys().collect();
    for k in unicorn.slots.keys() {
        if !vane.slots.contains_key(k) {
            names.push(k);
        }
    }
    names.sort();
    for name in names {
        let a = vane.slot(name);
        let b = unicorn.slot(name);
        if a != b {
            diffs.push(FieldDiff {
                name: name.clone(),
                vane: a,
                unicorn: b,
            });
        }
    }
    diffs
}

/// Render a full failure report: inputs, both engines' outcomes, and the
/// field diff. Deliberately self-contained so it can be pasted into an issue
/// and replayed with a unit test (plan §9).
pub fn report(
    title: &str,
    pc: u64,
    words: &[u32],
    init_regs: &[u64],
    vane_outcome: &str,
    unicorn_outcome: &str,
    diffs: &[FieldDiff],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "== cross-fuzz mismatch: {title} ==");
    let _ = writeln!(out, "pc: {pc:#x}");
    for (i, w) in words.iter().enumerate() {
        let _ = writeln!(out, "word[{i}]: {w:#010x}");
    }
    let _ = write!(out, "initial regs:");
    for (i, r) in init_regs.iter().enumerate() {
        if i % 4 == 0 {
            let _ = writeln!(out);
            let _ = write!(out, "  ");
        }
        let _ = write!(out, "x{i}={r:#x} ");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "vane:    {vane_outcome}");
    let _ = writeln!(out, "unicorn: {unicorn_outcome}");
    let _ = writeln!(out, "diffs ({}):", diffs.len());
    for d in diffs {
        d.render(&mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::BTreeMap, vec};

    fn names() -> [&'static str; 32] {
        core::array::from_fn(|i| match i {
            31 => "sp",
            _ => "x",
        })
    }

    fn state(regs: [u64; 32], slots: &[(&str, u64)]) -> MachineState {
        MachineState {
            regs,
            slots: slots
                .iter()
                .copied()
                .map(|(k, v)| (alloc::string::String::from(k), v))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn diff_reports_only_diverging_fields() {
        let mut a = state([0; 32], &[]);
        a.regs[3] = 1;
        let mut b = state([0; 32], &[]);
        b.regs[3] = 2;
        b.regs[5] = 9;
        let diffs = diff_states(&a, &b, &names());
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].name, "x3");
        assert_eq!(diffs[0].vane, 1);
        assert_eq!(diffs[0].unicorn, 2);
    }

    #[test]
    fn slot_missing_on_unicorn_side_reads_as_zero() {
        let a = state([0; 32], &[("rv:f1", 42)]);
        let b = state([0; 32], &[]);
        let diffs = diff_states(&a, &b, &names());
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].name, "rv:f1");
        assert_eq!(diffs[0].unicorn, 0);
    }

    #[test]
    fn report_is_self_describing() {
        let diffs = vec![FieldDiff {
            name: String::from("x1"),
            vane: 1,
            unicorn: 2,
        }];
        let r = report("t", 0x1000, &[0x123], &[5; 32], "TailCall(0x1004)", "ok", &diffs);
        assert!(r.contains("word[0]: 0x00000123"));
        assert!(r.contains("vane:    TailCall(0x1004)"));
        assert!(r.contains("x1"));
    }
}
