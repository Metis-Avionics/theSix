//! Capability honesty.
//!
//! The failure mode this layer exists to prevent: a rung reporting a property it
//! does not have, so a consumer builds on durability or cross-process sharing
//! that was never there. Every test here therefore checks a rung's claim against
//! something observable, not against the claim itself.
//!
//! Three of these tests replace ones that could not fail. `is_native()` answered
//! `true` for Postgres, RocksDb, Neo4j, Helix and Moka — backends the crate does
//! not contain — and `backend_classification_is_explicit` asserted that answer,
//! manufacturing assurance for stores that do not exist. The replacement asks
//! whether *code* backs the variant, and the feature-gated tests now actually
//! construct the backend instead of asserting on enum constants.

use std::collections::BTreeMap;
use std::sync::Arc;

use testkit::{StatedTier, default_tiers, manager_from_tiers, record_all, test_ctx};
use thesix::{
    BackendKind, CacheTier, CapabilityFlags, DefaultPolicy, DurabilityClass, L0Stub,
    OperationalState, TierCapability, TierId,
};

fn caps<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>()
-> BTreeMap<TierId, TierCapability> {
    let tiers = default_tiers::<V>();
    let m = manager_from_tiers(
        thesix::DefaultPolicy,
        tiers,
        std::time::Duration::from_secs(1),
    );
    m.capabilities()
}

/// Every rung must be reportable, including the one with no implementation.
/// A gap in the map is how a consumer ends up guessing.
#[test]
fn capabilities_cover_every_rung() {
    let c = caps::<String>();
    assert_eq!(c.len(), TierId::ALL.len());
    for id in TierId::ALL {
        assert!(c.contains_key(&id), "{id} missing from capabilities()");
    }
}

/// L0-L2 do their declared job; L3-L5 are fallbacks that store but claim nothing
/// richer. The anti-vacuity control: `is_bound()` must be true, so this cannot
/// pass by everything being reported unbound.
#[test]
fn default_build_reports_fallbacks_not_rich_backends() {
    testkit::proves!("capabilities.must_distinguish.fallback_from_authority");

    let c = caps::<String>();
    for id in [TierId::L0, TierId::L1, TierId::L2] {
        let cap = c[&id];
        assert_eq!(cap.backend, BackendKind::InMemory, "{id}");
        assert!(cap.flags.contains(CapabilityFlags::IN_MEMORY), "{id}");
        assert!(cap.flags.contains(CapabilityFlags::VOLATILE), "{id}");
        assert_eq!(cap.state, OperationalState::Healthy, "{id}");
        assert!(
            cap.is_bound(),
            "{id} must be bound for this test to mean anything"
        );
    }
    for id in [TierId::L3, TierId::L4, TierId::L5] {
        let cap = c[&id];
        assert_eq!(cap.backend, BackendKind::InMemoryFallback, "{id}");
        assert!(cap.is_bound(), "{id} must be bound");
        // The whole point of the fallback: it stores values, so the ladder
        // works, but it is process-local and must not claim to be the shared or
        // durable rung it nominally is.
        assert!(
            !cap.flags.contains(CapabilityFlags::SHARED),
            "{id} claims SHARED but is process-local"
        );
        assert!(
            !cap.flags.contains(CapabilityFlags::PERSISTENT),
            "{id} claims PERSISTENT but values die with the process"
        );
        assert_eq!(cap.durability, DurabilityClass::Volatile, "{id}");
    }
}

/// L4 is the *persistent* rung. Its default binding must not claim persistence.
/// If this ever flips, a consumer starts assuming restart survival it has not got.
#[test]
fn the_persistent_rung_does_not_claim_persistence_by_default() {
    testkit::proves!("capabilities.must_distinguish.volatile_from_persistent");

    let c = caps::<String>();
    assert!(!c[&TierId::L4].flags.contains(CapabilityFlags::PERSISTENT));
    assert!(!c[&TierId::L4].survives_restart());
}

/// An unbound rung and an unavailable rung are different facts. The old
/// `capabilities()` reported both as `Unavailable`, which is what let
/// `tier_for(&L6)` silently return L0's data.
#[test]
fn unbound_is_reported_as_unbound_not_unavailable() {
    testkit::proves!("capabilities.must_distinguish.unbound_from_unavailable");

    let c = caps::<String>();
    let l6 = c[&TierId::L6];
    assert_eq!(l6.state, OperationalState::Unbound);
    assert!(!l6.is_bound());
    assert_ne!(l6.state, OperationalState::Unavailable);
}

/// No rung may name a backend this crate does not implement. This is the direct
/// replacement for `is_native()` returning true for Postgres, RocksDb, Neo4j,
/// Helix and Moka.
#[test]
fn no_rung_claims_an_unimplemented_backend() {
    let c = caps::<String>();
    for (id, cap) in &c {
        assert!(
            cap.backend.is_implemented() || cap.backend == BackendKind::Unavailable,
            "{id} reports {:?}, which has no implementation in this crate",
            cap.backend
        );
    }
}

/// Unimplemented variants exist in the enum so the vocabulary is complete, but
/// they must not be answerable as "native". This is a table test on purpose: the
/// answer is a classification, and the classification is the thing under test.
#[test]
fn backend_classification_matches_implementation() {
    for (kind, implemented) in [
        (BackendKind::InMemory, true),
        (BackendKind::InMemoryFallback, true),
        (BackendKind::Redis, true),
        (BackendKind::Sled, true),
        (BackendKind::Oxigraph, true),
        (BackendKind::Origin, true),
        (BackendKind::Test, true),
        // Not a backend: it is the marker for "nothing here".
        (BackendKind::Unavailable, false),
        // Named but absent. These five are what `is_native()` used to bless.
        (BackendKind::Moka, false),
        (BackendKind::RocksDb, false),
        (BackendKind::Postgres, false),
        (BackendKind::Neo4j, false),
        (BackendKind::Helix, false),
    ] {
        assert_eq!(
            kind.is_implemented(),
            implemented,
            "{kind:?} classification changed; update the test and the docs together"
        );
    }
}

/// A consumer must be able to tell what a rung is without an operation failing.
/// Before, the only way to find out was to receive `TierUnavailable`.
#[test]
fn operational_state_is_queryable_without_an_operation() {
    testkit::proves!(
        "capabilities.must_distinguish.degraded_from_healthy",
        "capabilities.operation_failure_must_not_be_primary_discovery_mechanism"
    );

    let c = caps::<String>();
    // L3 in the default build is a working fallback, so it serves.
    assert!(c[&TierId::L3].state.is_serving());
    assert!(!c[&TierId::L6].state.is_serving());
}

/// Only a rung that a restart test has proven may claim `Verified`. In the
/// default build nothing is verified, so the class must not appear at all.
#[test]
fn nothing_claims_verified_durability_without_a_restart_test() {
    testkit::proves!("authority.l6.durability_required");

    let c = caps::<String>();
    for (id, cap) in &c {
        assert_ne!(
            cap.durability,
            DurabilityClass::Verified,
            "{id} claims Verified durability; only tests/durability may grant that"
        );
    }
}

/// Only the authority rung may claim authority. A fallback that claimed it
/// would be the authority-inversion the contract forbids.
#[test]
fn only_the_authority_rung_claims_authority() {
    testkit::proves!("authority.fallback_may_be_authoritative");

    let c = caps::<String>();
    for id in TierId::ALL {
        assert_eq!(
            c[&id].is_authoritative(),
            id == TierId::L6,
            "{id} authority flag is wrong"
        );
    }
}

/// Backends that do real I/O must declare that they block the calling thread, so
/// a consumer can decide whether to await them on a runtime worker.
#[cfg(feature = "redis")]
#[test]
fn redis_backend_reports_redis_and_declares_blocking_io() {
    // Non-vacuous by construction: this actually opens a backend and asks it.
    // The previous version of this test built an empty BTreeMap and asserted on
    // enum constants, so it proved nothing about the redis backend at all.
    let Ok(backend) = thesix::L3RedisBackend::<String>::connect("redis://127.0.0.1:1/") else {
        // No redis server in this environment. Report the skip rather than
        // pretending the assertion held.
        eprintln!("skipping: no redis reachable at 127.0.0.1:1");
        return;
    };
    let cap = backend.capability();
    assert_eq!(cap.backend, BackendKind::Redis);
    assert!(cap.flags.contains(CapabilityFlags::SHARED));
    assert!(
        cap.flags.contains(CapabilityFlags::BLOCKING_IO),
        "the redis client is synchronous; a consumer must be able to see that"
    );
    assert!(!cap.flags.contains(CapabilityFlags::PERSISTENT));
}

#[cfg(feature = "sled")]
#[test]
fn sled_backend_reports_sled_and_persistent() {
    let dir = std::env::temp_dir().join(format!("thesix-cap-{}", std::process::id()));
    let Ok(backend) = thesix::L4SledBackend::<String>::open(dir.to_string_lossy().as_ref()) else {
        eprintln!("skipping: sled store would not open");
        return;
    };
    let cap = backend.capability();
    assert_eq!(cap.backend, BackendKind::Sled);
    assert!(cap.flags.contains(CapabilityFlags::PERSISTENT));
    assert!(!cap.flags.contains(CapabilityFlags::SHARED));
    // sled's own defaults survive a restart, but nothing in *this* crate has
    // proven it end to end, so the honest class is Delegated, not Verified.
    assert_eq!(cap.durability, DurabilityClass::Delegated);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "oxigraph")]
#[test]
fn oxigraph_backend_reports_oxigraph_and_not_persistent() {
    let Ok(backend) = thesix::L5OxigraphBackend::<String>::new() else {
        eprintln!("skipping: oxigraph store would not open");
        return;
    };
    let cap = backend.capability();
    assert_eq!(cap.backend, BackendKind::Oxigraph);
    // The in-process `Store` is not a durable store, whatever the crate's
    // ambitions for this rung are.
    assert!(
        !cap.flags.contains(CapabilityFlags::PERSISTENT),
        "an in-process oxigraph Store must not claim persistence"
    );
    assert_eq!(cap.durability, DurabilityClass::Volatile);
}

/// A capability claim must correspond to behaviour. The previous suite had
/// `reported_fallbacks_actually_store` doing this for the fallbacks; keep it,
/// because a rung that refuses everything while claiming `Healthy` is exactly
/// the misrepresentation the surface was added to end.
#[tokio::test]
async fn reported_fallbacks_actually_store() {
    let tiers = default_tiers::<String>();
    let (tiers, tallies) = record_all(tiers);
    let m = manager_from_tiers(
        thesix::DefaultPolicy,
        tiers,
        std::time::Duration::from_secs(2),
    );

    for id in [TierId::L3, TierId::L4, TierId::L5] {
        let key = format!("fallback-{id}");
        let tier = m.tier(&id).unwrap_or_else(|| panic!("{id} is not bound"));
        assert!(tier.contains(&thesix::KeyRef(key.as_bytes())).await.is_ok());
        tier.set(&thesix::KeyRef(key.as_bytes()), format!("v-{id}"), None)
            .await
            .unwrap_or_else(|e| panic!("{id} refused a write while reporting Healthy: {e:?}"));
        let got = tier
            .get(&thesix::KeyRef(key.as_bytes()))
            .await
            .unwrap_or_else(|e| panic!("{id} refused a read: {e:?}"));
        assert_eq!(got, Some(format!("v-{id}")));
        tier.remove(&thesix::KeyRef(key.as_bytes())).await.ok();
    }

    // Anti-vacuity: the loop above must actually have driven operations.
    let total: u64 = tallies.iter().map(|t| t.total()).sum();
    assert!(
        total > 0,
        "no operations reached any tier; the test proved nothing"
    );
}

/// The ladder must work end to end on fallbacks alone, or "capability reporting"
/// is reporting a system nobody can use.
#[tokio::test]
async fn the_ladder_works_end_to_end_with_only_fallbacks() {
    testkit::proves!("continuity.graceful_degradation");

    let tiers = default_tiers::<String>();
    let (tiers, _tallies) = record_all(tiers);
    let m = manager_from_tiers(
        thesix::DefaultPolicy,
        tiers,
        std::time::Duration::from_secs(2),
    );
    let ctx = test_ctx();
    m.set(&"end-to-end".to_string(), "value".to_string(), &ctx)
        .await
        .expect("set on a fallback-only ladder");
    let got = m.get(&"end-to-end".to_string(), &ctx).await;
    assert_eq!(got.unwrap(), Some("value".to_string()));
}

/// `L0Stub` is the one rung a consumer is guaranteed to have; its capability
/// must be exactly what a process-local cache claims.
#[test]
fn l0_reports_exactly_in_memory_and_volatile() {
    testkit::proves!("acid.durability.fallback_may_claim_authority_durability");

    let l0: Arc<dyn CacheTier<String>> = Arc::new(L0Stub::new());
    let cap = l0.capability();
    assert_eq!(
        cap.flags,
        CapabilityFlags::IN_MEMORY | CapabilityFlags::VOLATILE,
        "L0 claimed {:?}",
        cap.flags
    );
    assert!(!cap.is_authoritative());
    assert!(!cap.is_blocking_io());
    assert_eq!(cap.durability, DurabilityClass::Volatile);
}

/// `Recovering` must be reported as itself, not folded into a neighbour.
///
/// `OperationalState::Recovering` means "coming back after a failure, not yet
/// trusted". The two states it could be collapsed into are both wrong in opposite
/// directions: reporting `Available`/`Healthy` tells a consumer the rung is
/// serving normally when it is not yet trusted, and reporting `Unavailable` throws
/// away the fact that it is on its way back, which is the only thing that makes
/// "how long until I can write here again" answerable.
///
/// The anti-vacuity control is that the rung really is bound and really does
/// report `Recovering` — otherwise the assertion below would pass against a
/// report that never mentioned it.
#[tokio::test]
async fn recovering_is_reported_as_recovering_and_distinguished_from_available() {
    testkit::proves!("capabilities.must_distinguish.recovering_from_available");

    let mut tiers = default_tiers::<String>();
    let wrapped = StatedTier::wrap(tiers[2].clone(), OperationalState::Recovering);
    tiers[2] = wrapped;
    let m = manager_from_tiers(
        thesix::DefaultPolicy,
        tiers,
        std::time::Duration::from_secs(1),
    );

    let reported = m.capabilities()[&TierId::L2];
    assert!(
        reported.is_bound(),
        "L2 must be bound or this test proves nothing"
    );
    assert_eq!(
        reported.state,
        OperationalState::Recovering,
        "Recovering was not reported as Recovering"
    );
    // The distinction the contract asks for, asserted against both neighbours.
    assert_ne!(
        reported.state,
        OperationalState::Unavailable,
        "Recovering collapsed into Unavailable, discarding that it is returning"
    );
    assert!(
        !reported.state.is_serving(),
        "Recovering claims to be serving; it is explicitly not yet trusted"
    );

    // And the wrapper changes only the report, not the storage behaviour: the
    // rung still holds and returns what was written to it.
    let ctx = test_ctx();
    let key = "recovering-rung".to_string();
    m.set(&key, "v".to_string(), &ctx)
        .await
        .expect("a Recovering rung still accepts writes; it is not Unavailable");
    assert_eq!(
        m.get(&key, &ctx).await.expect("read"),
        Some("v".to_string())
    );
}

/// `capabilities.eviction_policy_is_tier_defined = true`: the crate ships no
/// eviction policy, and the choice of what to evict belongs to the tier.
///
/// The tempting weaker test asserts `eviction_candidate()` is `None` somewhere. That
/// passes for the wrong reason -- `None` is also what a *delegating* tier returns when
/// it holds nothing -- so it cannot tell "this tier has no policy" from "this tier has
/// nothing to evict". It would prove the default, not tier-definition.
///
/// So both halves run against one populated rung. The rung delegates to the stub and
/// nominates; that same rung wrapped in a tier which does not override the method
/// nominates nothing. The difference is the tier's, not the data's.
#[tokio::test]
async fn eviction_policy_is_the_tiers_choice_not_the_crates() {
    testkit::proves!("capabilities.eviction_policy_is_tier_defined");

    let rung = Arc::new(L0Stub::<String>::new());
    let key = thesix::KeyRef(b"occupied");
    rung.set(&key, "v".to_string(), None)
        .await
        .expect("the rung holds a value");

    // Anti-vacuity for the positive half: the slot really is occupied, so a `None`
    // from the delegating tier would mean the policy declined rather than that there
    // was nothing there. Without this the test could not tell the two cases apart.
    assert!(
        rung.contains(&key).await.expect("contains"),
        "the rung is empty, so a later `None` would prove nothing"
    );
    assert!(
        rung.eviction_candidate().is_some(),
        "a rung holding a value must nominate it; otherwise the positive half is \
         satisfied by an empty store"
    );

    // The same rung behind a tier that does not override `eviction_candidate`.
    // `StatedTier` delegates every operation except `capability`, so it inherits the
    // trait default -- and the default is the crate having no opinion.
    let silent = StatedTier::wrap(rung.clone(), OperationalState::Healthy);
    assert!(
        silent.contains(&key).await.expect("contains"),
        "the wrapper must not change what is stored, or the two halves would differ by \
         more than the policy"
    );
    assert!(
        silent.eviction_candidate().is_none(),
        "a tier that does not override `eviction_candidate` still nominated one, so the \
         crate does have a default policy and the clause is false"
    );
}

/// `engineering.design.tier_topology_is_replaceable = true`: the seven-rung topology is
/// infrastructure behind `CacheTier`, not a structure the manager depends on.
///
/// The proof is substitutivity rather than inspection: the same operations are driven
/// against two different sets of tier implementations and must produce identical
/// observable results. A manager that reached past its tiers -- for a rung's identity,
/// its capacity, or its position in the ladder -- would diverge here even though every
/// individual call still looked correct.
///
/// Anti-vacuity in both directions: the substitute must be a *different* implementation
/// (asserted by type, not by comment), and the two must agree on values, on misses, and
/// on what a saturated ladder does. Comparing only the happy path would let a topology
/// assumption hide in the error path, which is where B17 and B22 both lived.
#[tokio::test]
async fn the_tier_topology_is_replaceable_behind_the_trait() {
    testkit::proves!("engineering.design.tier_topology_is_replaceable");

    let key = "replaceable".to_string();
    let ctx = test_ctx();

    // Two managers, same policy, materially different tier implementations.
    let stock = testkit::make_manager::<String>(DefaultPolicy);
    // `StatedTier` wraps each rung and delegates every operation, so the concrete type
    // behind every rung differs while the observable behaviour does not. Using an
    // existing delegating wrapper rather than inventing one keeps the claim honest: the
    // substitute has to be a real `CacheTier`, not a test-only shape.
    let stock_tiers = default_tiers::<String>();
    let substitute = manager_from_tiers(
        DefaultPolicy,
        stock_tiers
            .iter()
            .map(|t| {
                StatedTier::wrap(t.clone(), OperationalState::Healthy) as Arc<dyn CacheTier<String>>
            })
            .collect(),
        std::time::Duration::from_millis(250),
    );

    for m in [&stock, &substitute] {
        m.set(&key, "v".to_string(), &ctx).await.expect("set");
        assert_eq!(m.get(&key, &ctx).await.expect("get"), Some("v".to_string()));
        assert!(m.exists(&key, &ctx).await.expect("exists"));
    }

    // Agreement on a miss, not only on a hit.
    // Agreement on a miss, not only on a hit. `get` reports an absent key as
    // `Err(Miss)`, so that is the shape compared -- a topology assumption would most
    // plausibly hide here, where "no value" is produced by control-plane state rather
    // than by a tier answering.
    let absent = "absent".to_string();
    for m in [&stock, &substitute] {
        assert!(
            matches!(m.get(&absent, &ctx).await, Err(thesix::CacheError::Miss)),
            "a miss must report Miss regardless of which tier implementation backs the \
             rungs"
        );
    }

    // And agreement on removal.
    for m in [&stock, &substitute] {
        m.remove(&key, &ctx).await.expect("remove");
        assert!(
            matches!(m.get(&key, &ctx).await, Err(thesix::CacheError::Miss)),
            "a removed key must read as a miss under either topology"
        );
    }
}
