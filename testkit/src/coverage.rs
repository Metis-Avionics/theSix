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
    "eviction_reclaims_a_saturated_ladder",
    "cancelled_operation",
    "duplicate_operation",
    "stale_generation",
    "write_contended",
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

/// `[testing.property].invariants` paired with the test that proves it.
///
/// This registry exists because `PROPERTY_INVARIANTS` was a name list and nothing
/// more. `tests/contract` compared it against the contract for *set equality*,
/// which proves the two lists agree and not that any test exists. All nine
/// invariants were in fact covered — but the binding lived in two comments:
///
/// ```text
/// // no_deadlock / no_lock_across_await
/// // no_capability_misreporting / no_false_durability
/// ```
///
/// Two `proptest!` blocks each claimed two invariants. Deleting either block left
/// every gate green while the contract still asserted `no_deadlock`. A comment is
/// not a registry; it is a claim that outlives the thing it claims.
///
/// The mapping is one-to-many here, which is why `declare_cases!` cannot express
/// it: that macro emits `#[tokio::test]`, and these cases are `proptest!` blocks
/// whose generators cannot move into a macro arm. So the link is a table, and
/// `tests/contract` checks every cited function exists.
pub const PROPERTY_CASE_PROOFS: &[(&str, &str)] = &[
    (
        "no_silent_data_loss",
        "tests/property/main.rs::no_silent_data_loss",
    ),
    (
        "no_cross_key_corruption",
        "tests/property/main.rs::no_cross_key_corruption",
    ),
    (
        "no_authority_inversion",
        "tests/property/main.rs::no_authority_inversion",
    ),
    (
        "no_invalid_state_promotion",
        "tests/property/main.rs::no_invalid_state_promotion",
    ),
    (
        "no_deadlock",
        "tests/property/main.rs::control_plane_survives_a_wedged_data_plane",
    ),
    (
        "no_lock_across_await",
        "tests/property/main.rs::control_plane_survives_a_wedged_data_plane",
    ),
    (
        "no_capability_misreporting",
        "tests/property/main.rs::no_capability_misreporting",
    ),
    (
        "no_false_durability",
        "tests/property/main.rs::no_capability_misreporting",
    ),
    (
        "recovery_is_idempotent",
        "tests/property/main.rs::recovery_is_idempotent",
    ),
];

/// `[testing.fault_injection].faults` paired with the test that injects it.
///
/// The same gap as `PROPERTY_CASE_PROOFS`, one layer over. `FAULT_CLASSES` holds
/// the `FaultClass` names and was checked against the contract for set equality,
/// which again proves the lists agree rather than that a fault is ever fired.
/// Every one of the twelve did have a test, under a descriptive name that did not
/// match the contract's — which is exactly why nothing could check the
/// correspondence automatically.
pub const FAULT_CASE_PROOFS: &[(&str, &str)] = &[
    (
        "latency",
        "tests/fault_injection/main.rs::latency_stalls_without_failing",
    ),
    (
        "timeout",
        "tests/fault_injection/main.rs::timeout_fails_the_operation",
    ),
    (
        "hang",
        "tests/fault_injection/main.rs::hang_parks_the_operation_and_leaves_the_control_plane_free",
    ),
    (
        "read_failure",
        "tests/fault_injection/main.rs::read_failure_fails_a_read_only",
    ),
    (
        "write_failure",
        "tests/fault_injection/main.rs::write_failure_never_serves_the_replacement_and_leaves_no_intent",
    ),
    (
        "metadata_failure",
        "tests/fault_injection/main.rs::metadata_failure_fails_the_operation_but_not_the_entry",
    ),
    (
        "corruption",
        "tests/fault_injection/main.rs::corruption_is_detected_rather_than_served",
    ),
    (
        "disconnect",
        "tests/fault_injection/main.rs::disconnect_looks_like_an_unavailable_rung",
    ),
    (
        "capacity_exhaustion",
        "tests/fault_injection/main.rs::capacity_exhaustion_is_reported_as_such",
    ),
    (
        "failure_after_n_operations",
        "tests/fault_injection/main.rs::failure_after_n_operations_succeeds_first_then_fails",
    ),
    (
        "cancellation",
        "tests/fault_injection/main.rs::cancellation_releases_the_claim",
    ),
    (
        "partial_write",
        "tests/fault_injection/main.rs::the_partial_write_fault_really_stores_bytes_before_failing",
    ),
];

/// Every declared invariant paired with the test that proves it.
///
/// The contract names 140 leaves across nine sections. Before this registry
/// existed, *none* of them was bound to a test: the contract asserted that flags
/// were set, that layers had targets, that negative cases had cases — and never
/// that a declared invariant had anything exercising it. A new invariant therefore
/// cost nothing to add, which is the over-promise the contract was supposed to make
/// impossible.
///
/// Locators are `path::fn`, resolved against the repository root by
/// `tests/contract/invariant.rs`. `tests/contract` proves the registry is honest;
/// `theSix.toml` holds the *waivers*, so the test side owns what its tests cover
/// and the contract side owns what it admits to leaving uncovered. Neither file
/// can quietly satisfy the other.
///
/// Since B20 each cited test must also **declare** the clause it is cited for, via
/// `testkit::proves!`. Existence alone was the defect B20 closed: a row pointing at a
/// real but irrelevant test satisfied the gate, so the registry read as coverage while
/// proving nothing.
///
/// The limit that remains is irreducible: a test may declare the right clause and
/// assert the wrong thing. A declaration is a claim by the test about itself, so it can
/// be wrong in the same way any test can be wrong. What the gate does enforce is that a
/// clause cannot be added, renamed or deleted without this file changing with it, and
/// that a mis-citation fails rather than passing quietly.
pub const INVARIANT_PROOFS: &[(&str, &str)] = &[
    (
        "engineering.lock_across_await",
        "tests/property/main.rs::control_plane_survives_a_wedged_data_plane",
    ),
    (
        "verification.anti_vacuity_required",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.check_all_features",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.check_all_targets",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.clippy_warnings_as_errors",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.dependency_audit",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.dependency_hygiene",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.doctests",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.documentation",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.fmt",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.package_validation",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.tests_all_features",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.required.tests_all_targets",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "verification.verification_required",
        "tests/contract/main.rs::required_verification_flags_are_all_set",
    ),
    (
        "capabilities.eviction_policy_is_tier_defined",
        "tests/capability.rs::eviction_policy_is_the_tiers_choice_not_the_crates",
    ),
    (
        "cia.availability.write_race_exhaustion_reports_contended",
        "tests/negative/main.rs::write_contended",
    ),
    (
        "engineering.ci_actions_sha_pinned",
        "tests/contract/main.rs::every_ci_action_reference_is_pinned_by_sha",
    ),
    (
        "engineering.design.tier_topology_is_replaceable",
        "tests/capability.rs::the_tier_topology_is_replaceable_behind_the_trait",
    ),
    (
        "engineering.eviction_is_control_plane_authorised",
        "tests/concurrency.rs::an_entry_under_population_is_not_evictable",
    ),
    (
        "engineering.eviction_is_last_resort_after_degradation",
        "tests/soak/main.rs::the_manager_degrades_down_the_ladder_before_it_fails",
    ),
    (
        "engineering.eviction_may_discard_a_ready_entry",
        "tests/negative/main.rs::eviction_reclaims_a_saturated_ladder",
    ),
    (
        "engineering.fake_durability",
        "tests/durability/main.rs::verified_durability_is_earned_not_assumed",
    ),
    (
        "engineering.hidden_blocking",
        "tests/await_safety.rs::the_crates_own_source_carries_no_unsafe_and_no_blocking_calls",
    ),
    (
        "engineering.runtime_block_on",
        "tests/await_safety.rs::the_crates_own_source_carries_no_unsafe_and_no_blocking_calls",
    ),
    (
        "engineering.rust_memory_safety_required",
        "tests/await_safety.rs::the_crates_own_source_carries_no_unsafe_and_no_blocking_calls",
    ),
    (
        "engineering.silent_backend_substitution",
        "tests/l6_authority.rs::unbound_l6_is_reported_not_substituted",
    ),
    (
        "engineering.unnecessary_dependencies",
        "tests/contract/workflow.rs::every_declared_gate_is_executed_by_a_ci_job",
    ),
    (
        "engineering.unsafe_requires_justification",
        "tests/await_safety.rs::the_crates_own_source_carries_no_unsafe_and_no_blocking_calls",
    ),
    (
        "verification.adversarial.concurrency",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.durability",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.fault_injection",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.fuzz",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.negative",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.performance",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.property",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.recovery",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.security",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.adversarial.soak",
        "tests/contract/main.rs::every_adversarial_layer_is_enabled_and_bound_to_a_target",
    ),
    (
        "verification.anti_vacuity.blocked_state_must_be_verified",
        "tests/fault_injection/main.rs::hang_parks_the_operation_and_leaves_the_control_plane_free",
    ),
    (
        "verification.anti_vacuity.corruption_must_be_verified",
        "tests/fault_injection/main.rs::corruption_is_detected_rather_than_served",
    ),
    (
        "verification.anti_vacuity.fallback_path_must_be_verified",
        "tests/soak/main.rs::the_manager_degrades_down_the_ladder_before_it_fails",
    ),
    (
        "verification.anti_vacuity.fault_activation_must_be_verified",
        "tests/fault_injection/main.rs::latency_stalls_without_failing",
    ),
    (
        "verification.anti_vacuity.recovery_path_must_be_verified",
        "tests/recovery/main.rs::an_aborted_prepared_write_is_not_left_readable",
    ),
    (
        "verification.every_invariant_has_a_proof_or_waiver",
        "tests/contract/invariant.rs::every_declared_invariant_has_a_proof_or_a_waiver",
    ),
    (
        "verification.merge_readiness.tracker",
        "xtask/src/gates.rs::the_tracker_path_is_read_from_the_contract",
    ),
    (
        "concurrency.testing.promotion_eviction_races",
        "tests/concurrency.rs::eviction_invalidates_an_in_flight_commit",
    ),
    (
        "capabilities.must_distinguish.recovering_from_available",
        "tests/capability.rs::recovering_is_reported_as_recovering_and_distinguished_from_available",
    ),
    (
        "cia.availability.unbounded_retry",
        "tests/negative/main.rs::retry_counts_are_bounded_and_observed",
    ),
    (
        "cia.integrity.silent_stale_data_acceptance",
        "tests/integration.rs::stale_service_is_reported_rather_than_silent",
    ),
    (
        "concurrency.nested_block_on",
        "tests/await_safety.rs::nested_block_on_is_absent_from_the_crate_and_would_be_caught",
    ),
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

/// The repository root, for registries that cite files by relative path.
///
/// Shared so the anchor cannot differ between two callers: a `repo_root()` that
/// resolved to the *crate* directory in one place and the repository root in
/// another would make the same locator valid in one check and missing in the
/// other.
///
/// The `join("..")` is not incidental. `CARGO_MANIFEST_DIR` here is `testkit/`,
/// while every locator in these registries is spelled relative to the workspace
/// root (`tests/property/main.rs`). Dropping the `..` makes all three locators
/// resolve to `testkit/tests/...`, which is not a directory — and the failure
/// looks like "the contract cites phantom tests" rather than "this anchor is
/// wrong", which is the more expensive mistake of the two to diagnose.
#[must_use]
pub fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Resolve `path::fn` locators and report the ones that name nothing.
///
/// Shared by every registry that cites a test by location, so the existence rule
/// is written once. `tests/contract/invariant.rs` grew its own copy of this walk
/// first; a second and third copy is how the copies drift, and the drift would be
/// invisible because each would still be *checking* something.
///
/// The limit is worth stating plainly: this proves a function of that name is
/// defined in that file — not that it is a test, and not that it exercises what
/// the registry claims. Naming a real but irrelevant function satisfies it. That
/// is the same limit `INVARIANT_PROOFS` documents, and the reason the fault
/// layer's ledger assertions are the instrument that actually closes it.
#[must_use]
pub fn missing_locators(root: &std::path::Path, registry: &[(&str, &str)]) -> Vec<String> {
    let mut missing = Vec::new();
    for (name, locator) in registry {
        let Some((path, fn_name)) = locator.split_once("::") else {
            missing.push(format!(
                "{name}: malformed locator {locator:?}, want path::fn"
            ));
            continue;
        };
        let full = root.join(path);
        if !full.is_file() {
            missing.push(format!("{name}: {path} does not exist"));
            continue;
        }
        let source = match std::fs::read_to_string(&full) {
            Ok(s) => s,
            Err(e) => {
                missing.push(format!("{name}: {path} is unreadable: {e}"));
                continue;
            }
        };
        let needle = format!("fn {fn_name}");
        if !source
            .lines()
            .any(|l| l.trim_start().starts_with(&needle) || l.contains(&needle))
        {
            missing.push(format!("{name}: {locator} names no function"));
            continue;
        }

        // Existence is not citation. B20's whole point: a row may name a real
        // function that has nothing to do with the clause, and the locator
        // resolves, so the gate passes on an empty claim.
        //
        // The check is that the cited function declares the clause itself, via
        // `proves!`. Putting the label in the test means a mis-citation fails:
        // the wrongly cited test's own list does not contain this clause.
        //
        // The body is located rather than the whole file, so a label in a
        // *different* test in the same file cannot satisfy this row.
        if !declares_clause(function_span(&source, &needle), name) {
            missing.push(format!(
                "{name}: {locator} does not declare `proves!(\"{name}\")`. \
                 The locator resolves, so this row would otherwise assert the clause \
                 on the strength of an unrelated test."
            ));
        }
    }
    missing
}

/// The text belonging to the function whose signature line contains `needle`.
///
/// Delimited by line, not by matching braces. A brace scanner has to understand
/// char literals, lifetimes, raw and byte strings, and braces inside comments --
/// and a first attempt at one failed on exactly that, reporting a body it could
/// not close for a function that closes perfectly well. The rule here is
/// coarser and cannot fail that way: everything from the signature line up to the
/// next line that starts another function.
///
/// That is sufficient because `proves!` is always injected as a function's first
/// statement, before any nested item could appear. The failure mode is a missed
/// declaration, which fails the gate loudly, rather than a wrong span, which
/// would pass quietly.
fn function_span<'a>(source: &'a str, needle: &str) -> &'a str {
    let Some(at) = source.find(needle) else {
        return "";
    };
    let rest = &source[at..];
    let mut end = rest.len();
    // A byte offset accumulated by hand. `enumerate()` would supply an item
    // index, which is not a byte offset -- and using one here truncated the span
    // to sixteen characters, so every declaration looked absent.
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let at_line_start = offset == 0;
        offset += line.len();
        if at_line_start {
            continue;
        }
        let trimmed = line.trim_start();
        let starts_fn = [
            "fn ",
            "async fn ",
            "pub fn ",
            "pub async fn ",
            "unsafe fn ",
            "pub(crate) fn ",
        ]
        .iter()
        .any(|p| trimmed.starts_with(p));
        if starts_fn {
            end = offset;
            break;
        }
    }
    &rest[..end]
}

/// Whether `span` declares `clause` through `proves!` as a statement.
///
/// The opener must be the first thing on a trimmed line, which is what separates a
/// declaration from a comment naming one: `// proves!("clause")` contains the same
/// bytes and satisfied an earlier substring version of this check. That was not
/// hypothetical -- it is mutation 3 in B20's verification, and the gate passed with
/// the declaration commented out.
///
/// The arguments may span lines, because `cargo fmt` wraps the call whenever a test
/// proves several clauses, and a single-line parser then found an empty argument
/// list and reported every such row as undeclared. Comment lines *inside* the
/// argument list are skipped for the same reason as the opener.
fn declares_clause(span: &str, clause: &str) -> bool {
    let mut collecting = false;
    for line in span.lines() {
        let trimmed = line.trim_start();
        if !collecting {
            if let Some(rest) = trimmed
                .strip_prefix("testkit::proves!(")
                .or_else(|| trimmed.strip_prefix("proves!("))
            {
                collecting = true;
                if string_literals(rest).iter().any(|l| l == clause) {
                    return true;
                }
                if rest.contains(')') {
                    collecting = false;
                }
            }
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        if string_literals(trimmed).iter().any(|l| l == clause) {
            return true;
        }
        if trimmed.contains(')') {
            collecting = false;
        }
    }
    false
}

/// The string literals in `args`, up to the closing paren of the call.
///
/// Whole literals only, so a declared `a.b.c` cannot satisfy a claim on `a.b`.
fn string_literals(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = args.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            if c == ')' {
                break;
            }
            continue;
        }
        let mut value = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        value.push(escaped);
                    }
                }
                other => value.push(other),
            }
        }
        if closed {
            out.push(value);
        }
    }
    out
}

/// Declared case names that no proof registry claims.
///
/// The failure this exists to catch: a name present in both the contract and a
/// name-list registry, with nothing anywhere binding it to a test. Set equality
/// between the two lists cannot see that, because both lists are satisfied.
#[must_use]
pub fn unclaimed<'a>(declared: &[&'a str], registry: &[(&'a str, &'a str)]) -> Vec<&'a str> {
    let mut out = Vec::new();
    for name in declared {
        if !registry.iter().any(|entry| entry.0 == *name) {
            out.push(*name);
        }
    }
    out
}

/// Registry keys the contract no longer declares.
///
/// The other direction from `unclaimed`: a proof for something that stopped being
/// required is stale bookkeeping, and stale bookkeeping is how a registry grows
/// entries nobody maintains.
#[must_use]
pub fn overclaimed<'a>(declared: &[&'a str], registry: &[(&'a str, &'a str)]) -> Vec<&'a str> {
    let mut out = Vec::new();
    for entry in registry {
        if !declared.contains(&entry.0) {
            out.push(entry.0);
        }
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
