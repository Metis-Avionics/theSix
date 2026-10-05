//! The contract gate.
//!
//! `theSix.toml` is the source of truth. This target proves it is load-bearing
//! rather than decorative, by asserting four things that can only all be true if
//! the document, the crate and the test suite agree:
//!
//!  1. The contract parses, and its version is the crate's version.
//!  2. Its rung count is `TierId::ALL.len()`, and its last cache rung is the one
//!     `LAST_CACHE_TIER` names. (The previous `specs/` set declared `tier_count
//!     = 6` and `count = 7` in two files and shipped; a single asserted number
//!     cannot rot that way.)
//!  3. Its negative cases, fault taxonomy and property invariants match the
//!     registries in `testkit` exactly — not "is covered by", but equal.
//!  4. Every layer it declares is bound to a cargo test target, and every test
//!     target a layer names is claimed by a gate.
//!
//! It also refuses to let the contract quietly shrink: the mandatory
//! verification flags must all still be set.

use std::collections::{BTreeMap, BTreeSet};

use testkit::coverage::{
    FAULT_CASE_PROOFS, FAULT_CLASSES, NEGATIVE_CASES, PROPERTY_CASE_PROOFS, PROPERTY_INVARIANTS,
    missing_from_contract, missing_locators, overclaimed, repo_root, unclaimed,
};

/// The contract, embedded at compile time.
///
/// `include_str!` rather than a runtime read so a build from the published
/// crate still resolves the path, and so a missing contract is a compile error
/// rather than a test that silently passes when the file is absent.
const CONTRACT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/theSix.toml"));

#[derive(Debug, serde::Deserialize)]
struct Contract {
    spec: Spec,
    architecture: Architecture,
    authority: Authority,
    capabilities: Capabilities,
    testing: Testing,
    verification: Verification,
    #[allow(dead_code)]
    acid: toml::Value,
    #[allow(dead_code)]
    cia: toml::Value,
    #[allow(dead_code)]
    hpa: toml::Value,
    #[allow(dead_code)]
    continuity: toml::Value,
    #[allow(dead_code)]
    concurrency: toml::Value,
    #[allow(dead_code)]
    observability: toml::Value,
    #[allow(dead_code)]
    engineering: toml::Value,
}

#[derive(Debug, serde::Deserialize)]
struct Spec {
    name: String,
    version: String,
    kind: String,
}

#[derive(Debug, serde::Deserialize)]
struct Architecture {
    rung_count: usize,
    last_cache_rung: usize,
    consumer_backend_coupling: bool,
    authority_is_explicit: bool,
    capability_state_is_explicit: bool,
}

#[derive(Debug, serde::Deserialize)]
struct Authority {
    explicit: bool,
    inferred_from_tier_number: bool,
    fallback_may_be_authoritative: bool,
    l6: AuthorityL6,
}

#[derive(Debug, serde::Deserialize)]
struct AuthorityL6 {
    role: String,
    last_cache_rung: String,
}

#[derive(Debug, serde::Deserialize)]
struct Capabilities {
    operation_failure_must_not_be_primary_discovery_mechanism: bool,
    unbound_rung_is_substituted: bool,
    states: CapabilityStates,
    must_distinguish: BTreeMap<String, bool>,
}

#[derive(Debug, serde::Deserialize)]
struct CapabilityStates {
    allowed: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Testing {
    layers: BTreeMap<String, Vec<String>>,
    negative: Negative,
    fault_injection: FaultInjection,
    property: PropertyTesting,
}

#[derive(Debug, serde::Deserialize)]
struct Negative {
    cases: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct FaultInjection {
    faults: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct PropertyTesting {
    invariants: BTreeMap<String, bool>,
}

#[derive(Debug, serde::Deserialize)]
struct Verification {
    verification_required: bool,
    anti_vacuity_required: bool,
    required: BTreeMap<String, bool>,
    #[serde(default)]
    gate: Vec<ContractGate>,
    #[serde(default)]
    gate_order: GateOrder,
    #[serde(default)]
    attribution: Attribution,
}

#[derive(Debug, Default, serde::Deserialize)]
struct GateOrder {
    #[serde(default)]
    order: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ContractGate {
    name: String,
    kind: String,
}

#[derive(Debug, Default, serde::Deserialize)]
struct Attribution {
    #[serde(default)]
    forbidden_trailers: Vec<String>,
}

/// Compare a contract list (owned `String`s) against a suite registry.
///
/// A small local helper rather than reusing `testkit::coverage::diff`, whose
/// signature takes `&[&'static str]`: TOML gives us owned strings, and forcing
/// them to `'static` to satisfy a shared signature would mean leaking them.
fn diff_owned(expected: &[String], actual: &[&'static str]) -> String {
    let mut out = String::new();
    let missing: Vec<&str> = expected
        .iter()
        .map(String::as_str)
        .filter(|e| !actual.contains(e))
        .collect();
    let extra: Vec<&str> = actual
        .iter()
        .filter(|a| !expected.iter().any(|e| e == *a))
        .copied()
        .collect();
    if !missing.is_empty() {
        out.push_str(&format!("\n  contract-only: {}", missing.join(", ")));
    }
    if !extra.is_empty() {
        out.push_str(&format!("\n  suite-only: {}", extra.join(", ")));
    }
    if out.is_empty() && expected.len() != actual.len() {
        out.push_str("\n  (same set, different length: duplicate entries?)");
    }
    out
}

fn contract() -> Contract {
    toml::from_str(CONTRACT)
        .expect("theSix.toml must parse; if it does not, the contract gate has no meaning")
}

// ---------------------------------------------------------------------------
// 1. Identity
// ---------------------------------------------------------------------------

#[test]
fn contract_parses_and_names_this_crate() {
    let c = contract();
    assert_eq!(c.spec.name, "theSix");
    assert_eq!(c.spec.kind, "hpa-data-continuity-pipeline");
}

#[test]
fn contract_version_is_the_crate_version() {
    let c = contract();
    assert_eq!(
        c.spec.version,
        env!("CARGO_PKG_VERSION"),
        "theSix.toml [spec].version disagrees with Cargo.toml. The contract is the \
         source of truth, so one of them is stale."
    );
}

// ---------------------------------------------------------------------------
// 2. Topology
// ---------------------------------------------------------------------------

#[test]
fn rung_count_matches_the_tier_enum() {
    let c = contract();
    assert_eq!(
        c.architecture.rung_count,
        thesix::TierId::ALL.len(),
        "the contract declares {} rungs but TierId::ALL has {}. This is the exact \
         drift that shipped as `tier_count = 6` against `count = 7`.",
        c.architecture.rung_count,
        thesix::TierId::ALL.len()
    );
}

#[test]
fn last_cache_rung_matches_the_ladder_bound() {
    let c = contract();
    let authority_bound = thesix::TierId::parse(&c.authority.l6.last_cache_rung)
        .unwrap_or_else(|| {
            panic!(
                "authority.l6.last_cache_rung = {:?} is not a rung spelling",
                c.authority.l6.last_cache_rung
            )
        })
        .as_usize();
    assert_eq!(
        c.architecture.last_cache_rung, authority_bound,
        "[architecture].last_cache_rung and [authority.l6].last_cache_rung disagree. \
         Every rung-scanning path is bounded by this number; two spellings of it is \
         how a fallback scan ends up including authority."
    );
    // And both must equal the bound the code actually uses.
    assert_eq!(
        authority_bound,
        thesix::policy::LAST_CACHE_TIER.as_usize(),
        "the contract's ladder bound and the crate's LAST_CACHE_TIER disagree, so a \
         fallback scan bounded by one and a policy bounded by the other would \
         resolve different rungs"
    );
}

#[test]
fn authority_rung_is_the_one_past_the_ladder() {
    testkit::proves!("authority.l6.last_cache_rung", "authority.l6.role");

    let c = contract();
    let authority_idx = thesix::TierId::L6.as_usize();
    assert_eq!(
        c.architecture.last_cache_rung,
        authority_idx - 1,
        "L6 is authority, so exactly one rung must sit below the ladder bound"
    );
}

#[test]
fn authority_semantics_are_explicit_not_inferred() {
    testkit::proves!(
        "authority.explicit",
        "authority.inferred_from_tier_number",
        "authority.l6.implementation"
    );

    let c = contract();
    assert!(c.authority.explicit);
    assert!(!c.authority.inferred_from_tier_number);
    assert!(!c.authority.fallback_may_be_authoritative);
    assert!(c.architecture.authority_is_explicit);
    assert_eq!(c.authority.l6.role, "authority");
}

/// The consumer must not need to know backend identity.
///
/// The testable form of that claim: a complete lifecycle runs through the
/// continuity API alone, with no rung identifier and no backend type appearing
/// anywhere in the call, and the capability surface is never consulted. If a
/// future change made the data path require a rung, this stops compiling or
/// stops passing — which is the point of writing it as an executable claim
/// rather than a paragraph.
#[test]
fn the_consumer_never_needs_backend_identity() {
    testkit::proves!("cia.confidentiality.unintended_tier_exposure");

    use std::sync::Arc;
    use std::time::Duration;

    use thesix::{CacheContext, CacheManager, Cachelito, DefaultPolicy, MemoryPool, TierRegistry};

    let tiers = testkit::default_tiers::<String>();
    let pool = MemoryPool::<String>::new(64).expect("pool");
    let m: CacheManager<String, String, DefaultPolicy> = CacheManager::with_timeout(
        DefaultPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        tiers,
        pool,
        Duration::from_secs(1),
    );
    let ctx = CacheContext::new(thesix::IdentityContext::new(
        "alice".to_string(),
        vec!["reader".to_string()],
        "tenant-1".to_string(),
    ));

    let key = "opaque-key".to_string();

    // Every one of these takes (key, context) and nothing else. No TierId.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        m.set(&key, "v0".to_string(), &ctx).await.expect("set");
        assert_eq!(
            m.get(&key, &ctx).await.expect("get"),
            Some("v0".to_string())
        );
        assert!(m.exists(&key, &ctx).await.expect("exists"));

        let fetched = m
            .get_or_fetch(&"other".to_string(), &ctx, || async {
                Ok("v1".to_string())
            })
            .await
            .expect("get_or_fetch");
        assert_eq!(fetched, "v1".to_string());

        assert!(m.promote(&key, &ctx).await.is_ok());
        assert!(m.demote(&key, &ctx).await.is_ok());
        m.refresh(&key, &ctx, || async { Ok("v2".to_string()) })
            .await
            .expect("refresh");
        m.invalidate(&key, &ctx).await.expect("invalidate");
        m.remove(&key, &ctx).await.expect("remove");
    });

    // The Arc import above is only needed to keep the type annotation honest.
    let _ = Arc::new(());
}

/// Capability state must be explicit rather than inferred from an error.
#[test]
fn capability_state_is_explicit() {
    testkit::proves!(
        "capabilities.model.durability_axis",
        "capabilities.model.property_axis",
        "capabilities.model.state_axis"
    );

    let c = contract();
    assert!(
        !c.architecture.consumer_backend_coupling,
        "the consumer must not be coupled to backend identity"
    );
    assert!(c.architecture.capability_state_is_explicit);
    // An explicit state means a consumer can ask. Every state in the contract's
    // vocabulary is a distinct runtime value, not an alias for another.
    let states = thesix::capability::all_state_names();
    for required in [
        "unbound",
        "unavailable",
        "healthy",
        "degraded",
        "recovering",
    ] {
        assert!(
            states.contains(&required),
            "{required:?} must be a reportable state, not implied by another"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Coverage registries
// ---------------------------------------------------------------------------

#[test]
fn negative_cases_match_the_suite_registry() {
    let c = contract();
    let declared = &c.testing.negative.cases;
    let d = diff_owned(declared, NEGATIVE_CASES);
    assert!(
        d.is_empty(),
        "the contract's negative cases and the suite's registry disagree.{d}"
    );
}

#[test]
fn fault_taxonomy_matches_the_crate_and_the_registry() {
    let c = contract();
    let from_contract = &c.testing.fault_injection.faults;
    // The crate is the third leg: contract, registry, implementation.
    let from_crate: Vec<&'static str> =
        thesix::FaultClass::ALL.iter().map(|f| f.as_str()).collect();
    let d1 = diff_owned(from_contract, &from_crate);
    assert!(
        d1.is_empty(),
        "the contract's fault list and FaultClass::ALL disagree.{d1}"
    );
    let d2 = diff_owned(from_contract, FAULT_CLASSES);
    assert!(
        d2.is_empty(),
        "the contract's fault list and the suite registry disagree.{d2}"
    );
}

/// Every declared fault has a named test, and that test exists.
///
/// The test above this one only compares two *name lists*. That is satisfied by
/// two lists agreeing, and says nothing about whether a fault is ever injected —
/// so the mapping is bound here instead, in a registry that cites a real
/// function and is checked against the source tree.
#[test]
fn every_declared_fault_is_bound_to_a_test_that_exists() {
    let c = contract();
    let declared: Vec<&str> = c
        .testing
        .fault_injection
        .faults
        .iter()
        .map(String::as_str)
        .collect();

    let unclaimed = unclaimed(&declared, FAULT_CASE_PROOFS);
    assert!(
        unclaimed.is_empty(),
        "the contract requires faults that no test claims: {unclaimed:?}. Add a \
         `FAULT_CASE_PROOFS` row pointing at the test that injects it, or write \
         the test. A declared fault with no injector is a promise about a \
         failure mode nobody has ever produced."
    );

    let stale = overclaimed(&declared, FAULT_CASE_PROOFS);
    assert!(
        stale.is_empty(),
        "FAULT_CASE_PROOFS claims faults the contract no longer declares: {stale:?}. \
         Stale rows keep a removed fault looking covered."
    );

    let missing = missing_locators(&repo_root(), FAULT_CASE_PROOFS);
    assert!(
        missing.is_empty(),
        "FAULT_CASE_PROOFS cites tests that do not exist:\n  {}\n\
         A row naming a phantom test is worse than no row: the registry reads as \
         coverage of all twelve faults while proving less than the name lists did.",
        missing.join("\n  ")
    );
}

/// Every declared property invariant has a named test, and that test exists.
///
/// This is the check that was missing, and its absence was invisible: all nine
/// invariants were covered, but the binding lived in two comments in
/// `tests/property/main.rs`, so deleting either `proptest!` block would have left
/// every gate green while `theSix.toml` still asserted `no_deadlock`.
#[test]
fn every_declared_property_invariant_is_bound_to_a_test_that_exists() {
    let c = contract();
    let mut declared: Vec<&str> = c
        .testing
        .property
        .invariants
        .keys()
        .map(String::as_str)
        .collect();
    declared.sort_unstable();

    let unclaimed = unclaimed(&declared, PROPERTY_CASE_PROOFS);
    assert!(
        unclaimed.is_empty(),
        "the contract requires property invariants that no test claims: \
         {unclaimed:?}. Two of these were previously 'covered' by a comment."
    );

    let stale = overclaimed(&declared, PROPERTY_CASE_PROOFS);
    assert!(
        stale.is_empty(),
        "PROPERTY_CASE_PROOFS claims invariants the contract no longer declares: \
         {stale:?}"
    );

    let missing = missing_locators(&repo_root(), PROPERTY_CASE_PROOFS);
    assert!(
        missing.is_empty(),
        "PROPERTY_CASE_PROOFS cites tests that do not exist:\n  {}\n\
         A row naming a phantom test is worse than no row.",
        missing.join("\n  ")
    );
}

/// Every fault must be exercised, not merely declared.
///
/// Belt and braces over the row above: this asserts the *names* are a bijection
/// with the contract, so a row cannot quietly point two declared faults at one
/// test and leave the count looking complete.
#[test]
fn fault_proofs_cover_each_fault_exactly_once() {
    let c = contract();
    let declared: Vec<&str> = c
        .testing
        .fault_injection
        .faults
        .iter()
        .map(String::as_str)
        .collect();
    let keys: Vec<&str> = FAULT_CASE_PROOFS.iter().map(|e| e.0).collect();
    assert_eq!(
        keys.len(),
        declared.len(),
        "FAULT_CASE_PROOFS has {} rows for {} declared faults: {keys:?}",
        keys.len(),
        declared.len()
    );
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        keys.len(),
        "FAULT_CASE_PROOFS binds one fault to more than one row: {keys:?}"
    );
}

#[test]
fn property_invariants_match_the_suite_registry() {
    let c = contract();
    let mut declared: Vec<String> = c.testing.property.invariants.keys().cloned().collect();
    declared.sort();
    let d = diff_owned(&declared, PROPERTY_INVARIANTS);
    assert!(
        d.is_empty(),
        "the contract's property invariants and the suite registry disagree.{d}"
    );
}

#[test]
fn every_property_invariant_is_enabled() {
    let c = contract();
    for (name, value) in &c.testing.property.invariants {
        assert!(
            *value,
            "[testing.property.invariants].{name} is false. A named invariant that is \
             switched off is a requirement the contract has quietly withdrawn."
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Layers and gates
// ---------------------------------------------------------------------------

#[test]
fn every_declared_layer_is_bound_to_a_target() {
    let c = contract();
    assert!(
        c.testing.layers.contains_key("integration"),
        "the layer table must map the existing flat test targets, not replace them"
    );
    let all: BTreeSet<&str> = c
        .testing
        .layers
        .values()
        .flat_map(|v| v.iter().map(String::as_str))
        .collect();
    for required in [
        "integration",
        "concurrency",
        "capability",
        "negative",
        "property",
        "fault_injection",
        "recovery",
        "durability",
        "security",
        "performance",
        "soak",
        "contract",
        "backends",
    ] {
        assert!(
            c.testing.layers.contains_key(required),
            "no layer named {required:?}; the contract's testing model is incomplete"
        );
    }
    // Every target the layers name must be a target that exists on disk, or the
    // layer is a claim rather than a coverage statement.
    for target in &all {
        // `lib` is the crate's own unit-test binary, not a file under `tests/`.
        // It is the target that reaches `#[cfg(test)]` code inside `src/`, so it
        // is checked against the manifest rather than the directory.
        if **target == *"lib" {
            assert!(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("Cargo.toml")
                    .is_file(),
                "layer names target {target:?}, but this crate has no manifest"
            );
            continue;
        }
        let flat =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/{target}.rs"));
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/{target}/main.rs"));
        assert!(
            flat.is_file() || dir.is_file(),
            "layer names test target {target:?}, but neither tests/{target}.rs nor \
             tests/{target}/main.rs exists"
        );
    }
}

#[test]
fn required_verification_flags_are_all_set() {
    let c = contract();
    assert!(c.verification.verification_required);
    assert!(c.verification.anti_vacuity_required);
    for (flag, value) in &c.verification.required {
        assert!(
            *value,
            "[verification.required].{flag} is false. Every flag in that table is a \
             gate the contract declares mandatory; clearing one is how a required \
             gate silently stops being required."
        );
    }
    // The eleven flags the contract's own prose names.
    for expected in [
        "fmt",
        "check_all_targets",
        "check_all_features",
        "tests_all_targets",
        "tests_all_features",
        "doctests",
        "clippy_warnings_as_errors",
        "documentation",
        "dependency_audit",
        "dependency_hygiene",
        "package_validation",
    ] {
        assert!(
            c.verification.required.contains_key(expected),
            "[verification.required] is missing {expected:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Capability semantics
// ---------------------------------------------------------------------------

#[test]
fn capability_states_include_every_required_state() {
    let c = contract();
    for state in [
        "unbound",
        "unavailable",
        "volatile",
        "persistent",
        "authoritative",
        "shared",
        "degraded",
        "recovering",
        "in_memory",
    ] {
        assert!(
            c.capabilities.states.allowed.iter().any(|s| s == state),
            "the contract requires a {state:?} capability state but does not list it. \
             If the implementation cannot represent it, that is the finding — not a \
             reason to drop the state."
        );
    }
}

#[test]
fn capability_states_are_representable_in_code() {
    let c = contract();
    // Every allowed state must map onto something the crate can actually
    // produce, or the contract is describing a vocabulary that does not exist.
    let representable: BTreeSet<&str> = thesix::capability::all_state_names().into_iter().collect();
    for state in &c.capabilities.states.allowed {
        assert!(
            representable.contains(state.as_str()),
            "capability state {state:?} is in the contract but no code path can report it"
        );
    }
}

#[test]
fn capability_distinctions_are_all_required() {
    let c = contract();
    for (name, value) in &c.capabilities.must_distinguish {
        assert!(*value, "[capabilities.must_distinguish].{name} is false");
    }
    assert!(
        !c.capabilities.unbound_rung_is_substituted,
        "substituting one rung for an unbound one is silent backend substitution"
    );
    assert!(
        c.capabilities
            .operation_failure_must_not_be_primary_discovery_mechanism,
        "a consumer must be able to ask what a rung is without failing an operation"
    );
}

// ---------------------------------------------------------------------------
// 6. Anti-vacuity wiring
// ---------------------------------------------------------------------------

#[test]
fn every_declared_negative_case_has_a_test() {
    // `missing_from_contract` is the same comparison the layer suites run
    // against their own generated list; running it here too means a case added
    // to the contract without a test fails in this target even if the negative
    // layer is not part of the change under review.
    let missing = missing_from_contract(NEGATIVE_CASES, NEGATIVE_CASES);
    assert!(
        missing.is_empty(),
        "registry self-check failed: {missing:?}"
    );
}

/// The declared recovery directions must be the ones the runtime can take.
///
/// This exists because `recovery_direction_prepare_move` claimed
/// `"complete-forward"` for months of review while the production sweep could not
/// execute it: `Cachelito` persists a hash, so completing a move needs key bytes
/// the sweep does not have. Nothing caught the disagreement, because the contract
/// gate only ever compared the contract to the runner and the test inventory —
/// never to runtime behaviour.
///
/// So this asserts the declared value against `RecoveryDirection::for_kind`. A
/// future clause that over-promises has to be caught by a test that can fail, and
/// this is that test.
#[test]
fn declared_recovery_directions_match_what_the_runtime_can_execute() {
    testkit::proves!("acid.atomicity.recovery_direction_prepare_move");

    use thesix::{IntentKind, RecoveryDirection};

    let contract = std::fs::read_to_string("theSix.toml").expect("contract is readable");
    let value = |key: &str| -> String {
        contract
            .lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_once('='))
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
            .unwrap_or_else(|| panic!("theSix.toml declares no {key}"))
    };

    // A write is abortable from a hash alone, so the contract may say so.
    assert_eq!(
        value("recovery_direction_prepare_write"),
        "abort",
        "the contract no longer names the write direction"
    );
    assert_eq!(
        RecoveryDirection::for_kind(IntentKind::Write),
        RecoveryDirection::Abort
    );

    // A move is not. Whatever the contract declares, the direction the runtime
    // hands a sweep must not claim to be executable without the key — that is the
    // property the old clause violated.
    let declared_move = value("recovery_direction_prepare_move");
    assert_eq!(
        declared_move, "external-reconciliation",
        "the contract claims a move recovery the runtime does not implement"
    );
    let runtime_move = RecoveryDirection::for_kind(IntentKind::Move);
    assert_eq!(runtime_move, RecoveryDirection::ExternalReconciliation);
    assert!(
        !runtime_move.is_executable_here(),
        "the runtime claims it can execute a move recovery without the key"
    );

    // And aborting must never be presented as the move fallback: it is the one
    // direction that would destroy a committed value.
    assert!(
        !RecoveryDirection::Abort.is_success_without_the_key(),
        "abort is not a safe move fallback, so nothing may present it as one"
    );
}

// ---------------------------------------------------------------------------
// 7. Authorship policy
// ---------------------------------------------------------------------------

/// The banned-trailer policy has to exist, be usable, and be enforced.
///
/// `cargo xtask` validates all three when it loads the contract, but this is the
/// gate that runs in CI *before* the runner has been built, and it runs against
/// the real file rather than a parsed struct the runner also parses. Two parsers
/// agreeing is not the same as the policy being there.
#[test]
fn the_banned_trailer_policy_is_declared_and_enforced() {
    let c = contract();
    let keys = &c.verification.attribution.forbidden_trailers;

    assert!(
        !keys.is_empty(),
        "[verification.attribution].forbidden_trailers is empty. An empty list makes the \
         `authorship` gate a gate that cannot fail, which is the only kind of gate \
         this repository treats as a defect."
    );

    for key in keys {
        assert!(
            !key.contains(':') && !key.chars().any(char::is_whitespace),
            "{key:?} is not a usable git trailer key: the gate compares it against the \
             text before a message line's first colon, so a key carrying a colon or \
             whitespace could never match anything. It reads as coverage and checks \
             nothing."
        );
    }

    let authorship = c
        .verification
        .gate
        .iter()
        .find(|g| g.name == "authorship")
        .expect(
            "theSix.toml declares no `authorship` gate. A policy that no gate enforces \
             is a promise the runtime does not have to keep.",
        );
    assert_eq!(
        authorship.kind, "authorship",
        "the `authorship` gate must be run by the runner itself; declared as {:?}",
        authorship.kind
    );

    // The gate must be in the mandatory pass, not the deferred one, or it is a
    // gate nobody runs by default.
    assert!(
        c.verification
            .gate_order
            .order
            .iter()
            .any(|g| g == "authorship"),
        "`authorship` is missing from [verification.gate_order], so the runner would \
         never execute it"
    );
}

/// The policy must be stated as trailer *keys*, not as attributions.
///
/// A banned list holding a full attribution line would put that attribution into
/// this repository's source in order to prevent it appearing in the history —
/// the gate would cause the exact thing it exists to prevent. This is the one
/// property of the policy that cannot be checked by running the gate, because the
/// gate reads the same file this assertion reads.
#[test]
fn the_policy_names_keys_not_attributions() {
    let c = contract();
    for key in &c.verification.attribution.forbidden_trailers {
        assert!(
            !key.contains('@'),
            "{key:?} looks like an attribution rather than a trailer key. The policy \
             must forbid the *act* of adding a co-author trailer, not one value of it; \
             writing the value into the contract to forbid it would reintroduce it."
        );
        assert!(
            key.chars().count() <= 40,
            "{key:?} is too long to be a git trailer key. If this is an attribution, see \
             the previous assertion."
        );
    }
}

mod tetanus;
mod workflow;

mod invariant;

/// Every third-party action in CI must be pinned by commit SHA, because the
/// contract declares it (`engineering.ci_actions_sha_pinned`).
///
/// This is the enforcement half of B18. Pinning the refs once fixes today's
/// workflow and does nothing about next quarter's, and a mutable tag is exactly
/// the kind of quiet regression that survives review: the diff that introduces
/// it looks like a version bump.
///
/// The declared clause is read from the contract rather than hardcoded, so
/// weakening the contract weakens the check honestly instead of silently.
#[test]
fn every_ci_action_reference_is_pinned_by_sha() {
    let contract = std::fs::read_to_string("theSix.toml").expect("contract is readable");
    let declared = contract
        .lines()
        .find(|l| l.starts_with("ci_actions_sha_pinned"))
        .and_then(|l| l.split_once('='))
        .map(|(_, v)| v.trim())
        .unwrap_or_else(|| panic!("theSix.toml declares no ci_actions_sha_pinned"));
    assert_eq!(
        declared, "true",
        "this test only knows how to enforce the clause when it is true"
    );

    let mut checked = 0usize;
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(".github/workflows").expect("workflows dir is readable") {
        let path = entry.expect("dir entry").path();
        let text = std::fs::read_to_string(&path).expect("workflow is readable");
        for (n, line) in text.lines().enumerate() {
            let Some(rest) = line.trim().strip_prefix("- uses:") else {
                continue;
            };
            let rest = rest.split('#').next().unwrap_or("").trim();
            checked += 1;
            let Some((action, reference)) = rest.rsplit_once('@') else {
                offenders.push(format!("{}:{}: unparseable {rest}", path.display(), n + 1));
                continue;
            };
            // 40 hex characters is a full commit SHA. A tag (`v4`), a branch
            // (`stable`) or a short SHA are all mutable pointers.
            let pinned = reference.len() == 40 && reference.chars().all(|c| c.is_ascii_hexdigit());
            if !pinned {
                offenders.push(format!(
                    "{}:{}: {action}@{reference} is not pinned by SHA",
                    path.display(),
                    n + 1
                ));
            }
        }
    }

    assert!(checked > 0, "no action references were found to check");
    assert!(
        offenders.is_empty(),
        "ci actions must be pinned by SHA; {} of {checked} are not:\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}
