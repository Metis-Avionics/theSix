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
    "partial_write",
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

/// Every declared invariant paired with the test that proves it.
///
/// The contract names 95 leaves across its seven semantic sections. Before this
/// registry existed, *none* of them was bound to a test: the contract asserted
/// that flags were set, that layers had targets, that negative cases had cases —
/// and never that a declared invariant had anything exercising it. A new
/// invariant therefore cost nothing to add, which is the over-promise the
/// contract was supposed to make impossible.
///
/// Locators are `path::fn`, resolved against the repository root by
/// `tests/contract/invariant.rs`. `tests/contract` proves the registry is honest;
/// `theSix.toml` holds the *waivers*, so the test side owns what its tests cover
/// and the contract side owns what it admits to leaving uncovered. Neither file
/// can quietly satisfy the other.
///
/// The limit worth stating plainly: this proves a test is *named and exists*, not
/// that it proves the invariant. Naming a test that does not exercise the clause
/// satisfies the gate. That gap is the same one `declare_cases!` documents — the
/// remaining step is a review judgement, and the anti-vacuity instruments are
/// what make it checkable. What the gate does enforce is that an invariant cannot
/// be added, renamed or deleted without this file changing with it.
pub const INVARIANT_PROOFS: &[(&str, &str)] = &[
    (
        "acid.atomicity.cancelled_operation_may_commit_partially",
        "tests/recovery/main.rs::an_aborted_prepared_write_is_not_left_readable",
    ),
    (
        "acid.atomicity.commit_protocol",
        "tests/loom/main.rs::concurrent_commit_and_abort_leave_a_settled_state",
    ),
    (
        "acid.atomicity.partial_commit_visible",
        "tests/fault_injection/main.rs::a_misreported_write_never_serves_its_residue",
    ),
    (
        "acid.atomicity.prepare_state",
        "tests/recovery/main.rs::a_prepared_entry_is_invisible_to_readers",
    ),
    (
        "acid.atomicity.read_of_uncommitted_entry",
        "tests/recovery/main.rs::a_prepared_entry_is_invisible_to_readers",
    ),
    (
        "acid.atomicity.recovery_direction_prepare_move",
        "tests/contract/main.rs::declared_recovery_directions_match_what_the_runtime_can_execute",
    ),
    (
        "acid.atomicity.recovery_direction_prepare_write",
        "tests/recovery/main.rs::an_aborted_prepared_write_is_not_left_readable",
    ),
    (
        "acid.atomicity.recovery_must_be_idempotent",
        "tests/recovery/main.rs::recovery_is_idempotent",
    ),
    (
        "acid.atomicity.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "acid.consistency.authority_inversion",
        "tests/property/main.rs::no_authority_inversion",
    ),
    (
        "acid.consistency.corruption_may_be_silently_accepted",
        "tests/property/main.rs::corruption_is_never_swallowed",
    ),
    (
        "acid.consistency.invalid_state_may_be_promoted",
        "tests/property/main.rs::no_invalid_state_promotion",
    ),
    (
        "acid.consistency.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "acid.consistency.stale_generation_may_overwrite_newer_generation",
        "tests/security/main.rs::a_stale_generation_is_rejected",
    ),
    (
        "acid.durability.committed_data_must_survive_required_failure_domain",
        "tests/durability/main.rs::a_committed_value_survives_a_restart",
    ),
    (
        "acid.durability.fallback_may_claim_authority_durability",
        "tests/capability.rs::l0_reports_exactly_in_memory_and_volatile",
    ),
    (
        "acid.durability.false_durability_claims",
        "tests/property/main.rs::no_capability_misreporting",
    ),
    (
        "acid.durability.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "acid.isolation.cross_key_interference",
        "tests/property/main.rs::no_cross_key_corruption",
    ),
    (
        "acid.isolation.deadlock_tolerance",
        "tests/property/main.rs::control_plane_survives_a_wedged_data_plane",
    ),
    (
        "acid.isolation.intermediate_state_visibility",
        "tests/recovery/main.rs::a_prepared_entry_is_invisible_to_readers",
    ),
    (
        "acid.isolation.lock_guard_across_await",
        "tests/await_safety.rs::control_plane_stays_responsive_while_a_tier_awaits",
    ),
    (
        "acid.isolation.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "acid.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "authority.authority_loss_must_be_observable",
        "tests/durability/main.rs::authority_loss_is_observable",
    ),
    (
        "authority.explicit",
        "tests/contract/main.rs::authority_semantics_are_explicit_not_inferred",
    ),
    (
        "authority.fallback_may_be_authoritative",
        "tests/capability.rs::only_the_authority_rung_claims_authority",
    ),
    (
        "authority.inferred_from_tier_number",
        "tests/contract/main.rs::authority_semantics_are_explicit_not_inferred",
    ),
    (
        "authority.l6.durability_required",
        "tests/capability.rs::nothing_claims_verified_durability_without_a_restart_test",
    ),
    (
        "authority.l6.implementation",
        "tests/contract/main.rs::authority_semantics_are_explicit_not_inferred",
    ),
    (
        "authority.l6.last_cache_rung",
        "tests/contract/main.rs::authority_rung_is_the_one_past_the_ladder",
    ),
    (
        "authority.l6.role",
        "tests/contract/main.rs::authority_rung_is_the_one_past_the_ladder",
    ),
    (
        "capabilities.model.durability_axis",
        "tests/contract/main.rs::capability_state_is_explicit",
    ),
    (
        "capabilities.model.property_axis",
        "tests/contract/main.rs::capability_state_is_explicit",
    ),
    (
        "capabilities.model.state_axis",
        "tests/contract/main.rs::capability_state_is_explicit",
    ),
    (
        "capabilities.must_distinguish.degraded_from_healthy",
        "tests/capability.rs::operational_state_is_queryable_without_an_operation",
    ),
    (
        "capabilities.must_distinguish.fallback_from_authority",
        "tests/capability.rs::default_build_reports_fallbacks_not_rich_backends",
    ),
    (
        "capabilities.must_distinguish.unbound_from_unavailable",
        "tests/capability.rs::unbound_is_reported_as_unbound_not_unavailable",
    ),
    (
        "capabilities.must_distinguish.volatile_from_persistent",
        "tests/capability.rs::the_persistent_rung_does_not_claim_persistence_by_default",
    ),
    (
        "capabilities.operation_failure_must_not_be_primary_discovery_mechanism",
        "tests/capability.rs::operational_state_is_queryable_without_an_operation",
    ),
    (
        "capabilities.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "capabilities.unbound_rung_is_substituted",
        "tests/property/main.rs::continuity_never_claims_an_unbound_rung_is_healthy",
    ),
    (
        "cia.availability.control_plane_blocked_by_data_plane",
        "tests/await_safety.rs::control_plane_stays_responsive_while_a_tier_awaits",
    ),
    (
        "cia.availability.recovery_required",
        "tests/recovery/main.rs::the_sweep_finds_a_generation_zero_intent",
    ),
    (
        "cia.availability.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "cia.availability.single_tier_failure_must_cascade",
        "tests/recovery/main.rs::one_failing_rung_does_not_cascade",
    ),
    (
        "cia.availability.unbounded_queue_growth",
        "tests/soak/main.rs::sustained_traffic_with_faults_keeps_serving",
    ),
    (
        "cia.confidentiality.cross_key_data_exposure",
        "tests/security/main.rs::keys_within_a_tenant_are_isolated",
    ),
    (
        "cia.confidentiality.cross_tenant_access",
        "tests/security/main.rs::one_tenant_cannot_overwrite_another",
    ),
    (
        "cia.confidentiality.key_identity_is_non_reversible",
        "tests/security/main.rs::key_identity_is_not_reversible_by_construction",
    ),
    (
        "cia.confidentiality.payload_in_error_messages",
        "tests/security/main.rs::errors_carry_no_payload_or_key",
    ),
    (
        "cia.confidentiality.payload_in_telemetry",
        "tests/security/main.rs::telemetry_carries_no_payload_and_no_key",
    ),
    (
        "cia.confidentiality.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "cia.confidentiality.unintended_tier_exposure",
        "tests/contract/main.rs::the_consumer_never_needs_backend_identity",
    ),
    (
        "cia.integrity.corrupt_data_promoted",
        "tests/fault_injection/main.rs::corruption_is_detected_rather_than_served",
    ),
    (
        "cia.integrity.digest",
        "src/integrity.rs::seeded_hasher_matches_the_byte_lane_it_mirrors",
    ),
    (
        "cia.integrity.digest_is_cryptographic",
        "src/integrity.rs::blanket_digest_is_two_seeded_fnv_lanes_over_the_hash_stream",
    ),
    (
        "cia.integrity.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "cia.integrity.silent_corruption",
        "tests/security/main.rs::a_corrupt_value_is_never_served",
    ),
    (
        "cia.integrity.silent_generation_conflict",
        "tests/stampede.rs::test_stale_generation_rejection",
    ),
    (
        "cia.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "concurrency.cancel_safe",
        "tests/recovery/main.rs::a_cancelled_set_releases_its_commit_intent",
    ),
    (
        "concurrency.deadlock_free",
        "tests/property/main.rs::control_plane_survives_a_wedged_data_plane",
    ),
    (
        "concurrency.lock_guard_across_await",
        "tests/await_safety.rs::control_plane_stays_responsive_while_a_tier_awaits",
    ),
    (
        "concurrency.race_free",
        "tests/loom/main.rs::concurrent_commit_and_abort_leave_a_settled_state",
    ),
    (
        "concurrency.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "concurrency.testing.adversarial_scheduler",
        "tests/loom/main.rs::concurrent_commit_and_abort_leave_a_settled_state",
    ),
    (
        "concurrency.testing.cancellation_races",
        "tests/recovery/main.rs::a_cancelled_write_does_not_wedge_the_population_path",
    ),
    (
        "concurrency.testing.concurrent_reads",
        "tests/concurrency.rs::test_concurrent_readers",
    ),
    (
        "concurrency.testing.concurrent_writes",
        "tests/concurrency.rs::test_concurrent_writer_readers",
    ),
    (
        "concurrency.testing.loom_scope",
        "tests/loom/main.rs::the_shard_lock_serialises_independent_keys_correctly",
    ),
    (
        "concurrency.testing.random_interleavings",
        "tests/property/main.rs::generations_never_regress",
    ),
    (
        "concurrency.testing.read_write_races",
        "tests/concurrency.rs::test_concurrent_writer_readers",
    ),
    (
        "concurrency.testing.recovery_traffic_overlap",
        "tests/recovery/main.rs::continuity_is_observable_under_traffic",
    ),
    (
        "continuity.backend_failure_isolation",
        "tests/fault_injection/main.rs::one_failing_rung_does_not_take_the_others_down",
    ),
    (
        "continuity.explicit_state_transitions",
        "tests/recovery/main.rs::entry_states_classify_into_the_machine",
    ),
    (
        "continuity.graceful_degradation",
        "tests/capability.rs::the_ladder_works_end_to_end_with_only_fallbacks",
    ),
    (
        "continuity.reconciliation",
        "tests/recovery/main.rs::an_unresolvable_move_is_reported_separately_from_a_failure",
    ),
    (
        "continuity.recovery.idempotent",
        "tests/recovery/main.rs::recovery_is_idempotent",
    ),
    (
        "continuity.recovery.partial_recovery_safe",
        "tests/recovery/main.rs::concurrent_recovery_is_safe",
    ),
    (
        "continuity.recovery.recovery_failure_must_be_observable",
        "tests/recovery/main.rs::recovery_outcomes_are_distinguishable",
    ),
    (
        "continuity.recovery.repeatable",
        "tests/recovery/main.rs::recovery_is_idempotent",
    ),
    (
        "continuity.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
    (
        "hpa.control_plane.global_data_plane_lock",
        "tests/await_safety.rs::a_parked_tier_does_not_block_other_keys",
    ),
    (
        "hpa.control_plane.must_remain_responsive_during_data_plane_stall",
        "tests/await_safety.rs::control_plane_stays_responsive_while_a_tier_awaits",
    ),
    (
        "hpa.isolation.hot_key_may_block_unrelated_keys",
        "tests/performance/main.rs::a_hot_key_does_not_starve_cold_keys",
    ),
    (
        "hpa.isolation.hot_shard_may_block_entire_pipeline",
        "tests/performance/main.rs::distinct_shards_do_not_serialise",
    ),
    (
        "hpa.isolation.slow_backend_may_block_control_plane",
        "tests/fault_injection/main.rs::hang_parks_the_operation_and_leaves_the_control_plane_free",
    ),
    (
        "hpa.required",
        "tests/contract/invariant.rs::every_semantic_section_is_marked_required",
    ),
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
