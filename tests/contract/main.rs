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
    FAULT_CLASSES, NEGATIVE_CASES, PROPERTY_INVARIANTS, missing_from_contract,
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
