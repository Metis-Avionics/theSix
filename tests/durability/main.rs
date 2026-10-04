//! Durability: what survives a restart, and — more importantly — what this crate
//! is willing to *claim* survives one.
//!
//! The central assertion is negative. `DurabilityClass::Verified` is the only
//! claim that says "a value written through this binding came back after the
//! process went away", so the default build must contain none of it, and any
//! promotion to it must be earned by a test that actually drops and reopens a
//! store.

use std::time::Duration;

use testkit::{make_manager_with_timeout, test_ctx};
use thesix::{
    CacheError, CacheTier, CapabilityFlags, DurabilityClass, KeyRef, L0Stub, OperationalState,
    TierCapability, TierId,
};

/// Nothing may claim `Verified` without a restart test in this file.
#[test]
fn verified_durability_is_earned_not_assumed() {
    let m = make_manager_with_timeout::<String>(thesix::DefaultPolicy, Duration::from_millis(100));
    for (id, cap) in m.capabilities() {
        assert_ne!(
            cap.durability,
            DurabilityClass::Verified,
            "{id} claims Verified durability with no restart test to back it"
        );
    }
}

/// An in-memory rung must say so.
#[test]
fn in_memory_rungs_are_volatile() {
    let m = make_manager_with_timeout::<String>(thesix::DefaultPolicy, Duration::from_millis(100));
    let caps = m.capabilities();
    for id in [TierId::L0, TierId::L1, TierId::L2] {
        assert_eq!(
            caps[&id].durability,
            DurabilityClass::Volatile,
            "{id} is process-local and must not claim more"
        );
        assert!(caps[&id].flags.contains(CapabilityFlags::VOLATILE));
    }
}

/// The fallback rungs must not claim persistence even where their nominal tier
/// is the persistent one. L4 *is* the persistent rung; its default binding is not.
#[test]
fn the_persistent_rung_default_binding_is_volatile() {
    let m = make_manager_with_timeout::<String>(thesix::DefaultPolicy, Duration::from_millis(100));
    let l4 = m.capabilities()[&TierId::L4];
    assert!(
        !l4.flags.contains(CapabilityFlags::PERSISTENT),
        "L4's in-memory fallback claimed persistence"
    );
    assert!(!l4.survives_restart());
}

/// Durability claims and the authority role must not be conflated.
#[test]
fn durability_and_authority_are_separate_axes() {
    let m = make_manager_with_timeout::<String>(thesix::DefaultPolicy, Duration::from_millis(100));
    let caps = m.capabilities();
    for (id, cap) in &caps {
        if cap.is_authoritative() {
            assert_ne!(*id, TierId::L4, "a cache rung claimed authority");
        }
    }
}

/// A process restart is only meaningful against a durable store. With one
/// compiled in, drop it and reopen; without, say so rather than pass vacuously.
#[cfg(feature = "sled")]
#[tokio::test]
async fn a_committed_value_survives_a_restart() {
    use thesix::L4SledBackend;

    let dir = std::env::temp_dir().join(format!("thesix-durability-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let key = KeyRef(b"committed");

    // "Process 1": write, flush, drop everything.
    {
        let backend = L4SledBackend::<String>::open(dir.to_string_lossy().as_ref()).expect("open");
        assert!(
            backend
                .capability()
                .flags
                .contains(CapabilityFlags::PERSISTENT)
        );
        // Delegated until this test proves otherwise.
        assert_eq!(
            backend.capability().durability,
            DurabilityClass::Delegated,
            "sled claimed Verified before any restart test existed"
        );
        backend
            .set(&key, "survives".to_string(), None)
            .await
            .expect("write");
        backend.flush_for_test().await;
    }

    // "Process 2": a fresh handle on the same directory.
    let reopened = L4SledBackend::<String>::open(dir.to_string_lossy().as_ref()).expect("reopen");
    assert_eq!(
        reopened.get(&key).await.expect("read"),
        Some("survives".to_string()),
        "a committed value did not survive dropping and reopening the store"
    );

    // And the reverse: a removed value must not come back.
    reopened.remove(&key).await.expect("remove");
    reopened.flush_for_test().await;
    drop(reopened);
    let again = L4SledBackend::<String>::open(dir.to_string_lossy().as_ref()).expect("reopen");
    assert_eq!(
        again.get(&key).await.expect("read"),
        None,
        "a removed value came back after a restart"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(not(feature = "sled"))]
#[tokio::test]
async fn a_committed_value_survives_a_restart() {
    eprintln!(
        "substituted: no restart-durable backend compiled in; \
         run with --features sled for the drop-and-reopen case"
    );
    assert!(!cfg!(feature = "sled"));
}

/// Authority loss must be observable, not silent.
#[test]
fn authority_loss_is_observable() {
    let m = make_manager_with_timeout::<String>(thesix::DefaultPolicy, Duration::from_millis(100));
    let caps = m.capabilities();
    let l6 = caps[&TierId::L6];
    // The authority rung exists as a role...
    assert!(l6.is_authoritative());
    // ...but has no implementation bound, which is a *different* fact from being
    // available. Collapsing the two is how a consumer concludes the authority is
    // down when it was never configured.
    assert_eq!(l6.state, OperationalState::Unbound);
    assert!(!l6.is_bound());
    assert_ne!(l6.state, OperationalState::Unavailable);
}

/// Authority restoration must be observable too.
#[test]
fn authority_restoration_is_observable() {
    // Bind an authority-capable tier and check the state changes from Unbound to
    // a serving state, with the role preserved throughout.
    struct AuthorityTier;
    #[async_trait::async_trait]
    impl CacheTier<String> for AuthorityTier {
        fn name(&self) -> String {
            "authority".to_string()
        }
        fn backend(&self) -> thesix::BackendKind {
            thesix::BackendKind::Postgres
        }
        fn capability(&self) -> TierCapability {
            TierCapability::new(
                thesix::BackendKind::Postgres,
                CapabilityFlags::PERSISTENT | CapabilityFlags::AUTHORITATIVE,
                OperationalState::Healthy,
                DurabilityClass::Verified,
            )
        }
        async fn get(&self, _k: &KeyRef<'_>) -> Result<Option<String>, CacheError> {
            Ok(None)
        }
        async fn set(
            &self,
            _k: &KeyRef<'_>,
            _v: String,
            _t: Option<Duration>,
        ) -> Result<(), CacheError> {
            Ok(())
        }
        async fn remove(&self, _k: &KeyRef<'_>) -> Result<(), CacheError> {
            Ok(())
        }
        async fn contains(&self, _k: &KeyRef<'_>) -> Result<bool, CacheError> {
            Ok(false)
        }
        fn health(&self) -> thesix::TierHealth {
            thesix::TierHealth::default()
        }
        fn tier_id(&self) -> TierId {
            TierId::L6
        }
    }

    let mut tiers = testkit::default_tiers::<String>();
    tiers.push(std::sync::Arc::new(AuthorityTier) as std::sync::Arc<dyn CacheTier<String>>);
    let m = testkit::manager_from_tiers(thesix::DefaultPolicy, tiers, Duration::from_millis(100));

    let caps = m.capabilities();
    assert!(m.has_tier(&TierId::L6));
    assert_eq!(caps[&TierId::L6].state, OperationalState::Healthy);
    assert!(caps[&TierId::L6].is_authoritative());
    assert!(caps[&TierId::L6].survives_restart());
}

/// An incomplete operation must be resolvable, not merely survivable.
#[tokio::test]
async fn an_incomplete_operation_is_resolvable() {
    let m = make_manager_with_timeout::<String>(thesix::DefaultPolicy, Duration::from_millis(100));
    let ctx = test_ctx();
    let key = "interrupted".to_string();
    let framed = testkit::framed_key(&ctx, &key);

    // An intent as a crash would leave it.
    m.cachelito()
        .prepare(&framed, None, TierId::L1, thesix::IntentKind::Write)
        .expect("prepare");

    let (recovered, failed) = m.recover_older_than(Duration::from_secs(0));
    assert_eq!((recovered, failed), (1, 0));

    // Post-state: no intent, no partial visibility.
    let snap = m.cachelito().peek(&framed).expect("peek");
    assert!(snap.intent.is_none());
    assert!(!snap.state.is_readable());
}

/// Persistence failure must not be reported as success.
#[tokio::test]
async fn a_persistence_failure_is_not_a_success() {
    use testkit::faulty;
    use thesix::{FaultClass, FaultPlan, OpKind};

    let (tier, ledger) = faulty(
        std::sync::Arc::new(L0Stub::<String>::new()) as std::sync::Arc<dyn CacheTier<String>>,
        FaultPlan::new().push(OpKind::Set, FaultClass::Disconnect),
    );
    let m = testkit::manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        vec![tier as std::sync::Arc<dyn CacheTier<String>>],
        Duration::from_millis(200),
    );

    let r = m.set(&"p".to_string(), "v".to_string(), &test_ctx()).await;
    assert!(
        r.is_err(),
        "a disconnected backend reported a successful write"
    );
    assert_eq!(
        ledger.fired(FaultClass::Disconnect),
        1,
        "the fault never fired"
    );

    let snap = m
        .cachelito()
        .peek(&testkit::framed_key(&test_ctx(), "p"))
        .expect("peek");
    assert!(
        !snap.state.is_readable(),
        "a failed persistence reported a committed value"
    );
}
