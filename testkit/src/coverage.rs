//! Coverage registries.
//!
//! Each list here is the single declaration of "this case is covered". The
//! contract names the cases it requires; this crate names the cases the suite
//! handles; `tests/contract` asserts the two agree, and each layer asserts its
//! own test count matches what it declared.
//!
//! That chain is what stops a required case from being satisfied by a test that
//! was renamed, deleted, or never written. Without it, "the contract lists
//! nineteen negative cases" and "the suite has nineteen negative tests" are two
//! unrelated claims that happen to share a number.
//!
//! The `declare_*!` macros generate a test function and a `COVERED` list from
//! one invocation, so the two cannot drift apart inside a layer.

/// The negative cases the contract requires, spelled exactly as
/// `[testing.negative].cases` spells them.
pub const NEGATIVE_CASES: &[&str] = &[
    "backend_unavailable",
    "backend_timeout",
    "backend_hang",
    "malformed_response",
    "malformed_payload",
    "corrupt_payload",
    "invalid_metadata",
    "capacity_exhaustion",
    "cancelled_operation",
    "duplicate_operation",
    "stale_generation",
    "generation_conflict",
    "partial_write",
    "partial_promotion",
    "authority_unavailable",
    "fallback_unavailable",
    "recovery_failure",
    "process_restart",
    "invalid_configuration",
];

/// The fault classes the contract requires, spelled as
/// `[testing.fault_injection].faults` spells them. Cross-checked against
/// `thesix::FaultClass::ALL` by `cargo xtask contract`.
pub const FAULT_CLASSES: &[&str] = &[
    "latency",
    "timeout",
    "hang",
    "read_failure",
    "write_failure",
    "metadata_failure",
    "corruption",
    "disconnect",
    "capacity_exhaustion",
    "failure_after_n_operations",
    "cancellation",
];

/// The property invariants the contract requires.
pub const PROPERTY_INVARIANTS: &[&str] = &[
    "no_silent_data_loss",
    "no_cross_key_corruption",
    "no_authority_inversion",
    "no_invalid_state_promotion",
    "no_deadlock",
    "no_lock_across_await",
    "no_capability_misreporting",
    "no_false_durability",
    "recovery_is_idempotent",
];

/// Generate `#[tokio::test]` functions and a `COVERED` list from one invocation.
///
/// ```ignore
/// testkit::declare_cases! {
///     /// what this proves
///     pub fn covered_cases() -> &'static [&'static str] {
///         async fn backend_unavailable() { /* ... */ }
///         async fn backend_timeout() { /* ... */ }
///     }
/// }
/// ```
///
/// Every case is `async`, with no separate sync arm. Two arms would be nicer to
/// write, but `macro_rules!` does not backtrack across a repetition: an arm whose
/// repetition matches zero times succeeds with an empty list and errors on the
/// first token it cannot consume. A sync arm first would therefore swallow the
/// async cases. Uniform async with no awaits costs nothing.
///
/// The `COVERED` list is built from the same tokens as the functions, so a layer
/// cannot declare a case it does not test. The remaining gap — adding a case to
/// the invocation list without testing it properly — is a review question, and
/// the anti-vacuity instruments in this crate are what make the answer checkable.
#[macro_export]
macro_rules! declare_cases {
    (
        $(#[$outer:meta])*
        $vis:vis fn $covered_name:ident() -> &'static [&'static str] {
            $(
                $(#[$case_meta:meta])*
                async fn $case_name:ident() $body:block
            )*
        }
    ) => {
        $(
            $(#[$case_meta])*
            #[tokio::test]
            async fn $case_name() $body
        )*

        $vis fn $covered_name() -> &'static [&'static str] {
            &[$(stringify!($case_name)),*]
        }
    };
}

/// Assert a layer's declared cases are a subset of what the contract requires.
///
/// The direction matters: extra local cases are fine (a layer may test more than
/// the contract demands), but a required case with no test is a hole.
#[must_use]
pub fn missing_from_contract(
    required: &[&'static str],
    covered: &[&'static str],
) -> Vec<&'static str> {
    required
        .iter()
        .filter(|r| !covered.contains(r))
        .copied()
        .collect()
}

/// Compare two case lists and describe the difference in both directions, which
/// is more useful in a failure message than "assert failed".
#[must_use]
pub fn diff(expected: &[&'static str], actual: &[&'static str]) -> String {
    let mut out = String::new();
    let missing: Vec<&&str> = expected.iter().filter(|e| !actual.contains(e)).collect();
    let extra: Vec<&&str> = actual.iter().filter(|a| !expected.contains(a)).collect();
    if !missing.is_empty() {
        out.push_str(&format!(
            "\n  missing: {}",
            missing
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !extra.is_empty() {
        out.push_str(&format!(
            "\n  unexpected: {}",
            extra
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if out.is_empty() {
        out.push_str("\n  (lists match, but lengths differ: duplicate entries?)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registries_have_no_duplicates() {
        for (label, list) in [
            ("NEGATIVE_CASES", NEGATIVE_CASES),
            ("FAULT_CLASSES", FAULT_CLASSES),
            ("PROPERTY_INVARIANTS", PROPERTY_INVARIANTS),
        ] {
            let mut seen = std::collections::BTreeSet::new();
            for entry in list {
                assert!(
                    seen.insert(*entry),
                    "{label} lists {entry:?} twice, so its length overstates coverage"
                );
            }
        }
    }

    #[test]
    fn fault_registry_matches_the_crate_taxonomy() {
        let from_crate: Vec<&str> = thesix::FaultClass::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(FAULT_CLASSES, from_crate.as_slice());
    }

    #[test]
    fn missing_from_contract_reports_holes() {
        assert!(missing_from_contract(NEGATIVE_CASES, NEGATIVE_CASES).is_empty());
        let partial = &NEGATIVE_CASES[..3];
        let missing = missing_from_contract(NEGATIVE_CASES, partial);
        assert_eq!(missing.len(), NEGATIVE_CASES.len() - 3);
    }

    #[test]
    fn diff_names_both_directions() {
        let d = diff(&["a", "b"], &["b", "c"]);
        assert!(d.contains("missing: a"), "{d}");
        assert!(d.contains("unexpected: c"), "{d}");
    }
}
