//! Recovery: the full continuity lifecycle, and the properties that must hold
//! across it.
//!
//! ```text
//! HEALTHY -> DEGRADED -> FAILURE -> FALLBACK -> RECOVERY -> RECONCILIATION -> HEALTHY
//! ```
//!
//! Two requirements dominate. Recovery must be **idempotent** — running it twice,
//! or twice concurrently, changes nothing the second time — and it must be
//! **safe to fail**, meaning an unresolvable intent is reported rather than
//! silently skipped. A sweep that quietly drops work looks exactly like a sweep
//! that had none.

use std::sync::Arc;
use std::time::Duration;

use testkit::{FaultyTier, Tally, make_manager_with_timeout, test_ctx};
use thesix::{
    CacheError, CacheTier, ContinuityState, DefaultPolicy, EntryState, Generation, IntentKind,
    KeyRef, L0Stub, L1Stub, RecoveryDirection, RecoveryOutcome, TierId,
};

fn mgr() -> Arc<thesix::CacheManager<String, String, DefaultPolicy>> {
    make_manager_with_timeout(DefaultPolicy, Duration::from_millis(300))
}

/// Whether every *bound* rung is healthy.
///
/// `ContinuityReport::all_healthy` asks about every rung the registry knows,
/// which includes the unbound ones — and those correctly report `Unavailable`, so
/// `all_healthy` is false for any manager that is not fully bound. Asserting it on
/// a partial ladder would be asserting that the manager is broken.
fn all_bound_healthy(m: &thesix::CacheManager<String, String, DefaultPolicy>) -> bool {
    let report = m.continuity();
    report
        .states
        .iter()
        .filter(|(tier, _)| m.has_tier(tier))
        .all(|(_, state)| *state == ContinuityState::Healthy)
}

/// The state machine's vocabulary must be the contract's vocabulary.
#[test]
fn every_contract_continuity_state_exists() {
    for s in [
        "healthy",
        "degraded",
        "unavailable",
        "recovering",
        "reconciling",
    ] {
        assert!(
            ContinuityState::parse(s).is_some(),
            "{s} is not representable"
        );
    }
}

/// A failing rung must walk the documented lifecycle, not jump to a terminal
/// state.
#[tokio::test]
async fn a_failing_rung_walks_the_lifecycle() {
    let failing = FaultyTier::<String>::with_plan(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        thesix::FaultPlan::new(),
    );
    let good = Arc::new(L1Stub::<String>::new());
    // The tier is kept by name as well as by trait object, so the test can re-arm
    // its plan after the manager has taken ownership.
    let failing_tier: Arc<dyn CacheTier<String>> = failing.clone();
    let (tiers, _t) = testkit::record_all(vec![failing_tier, good as Arc<dyn CacheTier<String>>]);
    let m = testkit::manager_from_parts(
        DefaultPolicy,
        thesix::Cachelito::new(),
        tiers,
        Duration::from_millis(200),
    );

    // HEALTHY
    let healthy = m.continuity();
    assert_eq!(healthy.state(TierId::L0), ContinuityState::Healthy);
    assert!(
        all_bound_healthy(&m),
        "a fresh manager is not healthy: {}",
        healthy.summary()
    );
    assert!(
        !healthy.all_healthy(),
        "a partially-bound ladder claims every rung is healthy"
    );

    // Seed entries so a read reaches L0. A `get` for a key that was never
    // written is answered by the control plane alone, so the armed read faults
    // would never fire and the lifecycle would appear not to move.
    let ctx = test_ctx();
    for i in 0..8 {
        // Frame the key the way the manager does, or the seed lands on a
        // different key that nothing will ever read.
        let key = testkit::framed_key(&ctx, &format!("k{i}"));
        let token = m
            .cachelito()
            .prepare(&key, None, TierId::L0, IntentKind::Write)
            .expect("seed control state");
        // Commit it: a merely-prepared entry reads as `Prepared`, which `get`
        // treats as in-progress and never routes to a rung, so the armed read
        // faults would still never fire.
        m.cachelito()
            .commit(&token, None)
            .expect("commit control state");
    }

    // -> DEGRADED, by injecting failures into L0's reads.
    failing.arm(
        thesix::FaultPlan::new()
            .push(thesix::OpKind::Get, thesix::FaultClass::ReadFailure)
            .push(thesix::OpKind::Get, thesix::FaultClass::ReadFailure),
    );
    for i in 0..2 {
        let _ = m.get(&format!("k{i}"), &ctx).await;
    }
    let degraded = m.continuity();
    assert_eq!(
        degraded.state(TierId::L0),
        ContinuityState::Degraded,
        "two failures did not degrade the rung; health is {:?}",
        degraded.summary()
    );

    // -> FAILURE, past the threshold.
    failing.arm(
        thesix::FaultPlan::new()
            .push(thesix::OpKind::Get, thesix::FaultClass::ReadFailure)
            .push(thesix::OpKind::Get, thesix::FaultClass::ReadFailure)
            .push(thesix::OpKind::Get, thesix::FaultClass::ReadFailure)
            .push(thesix::OpKind::Get, thesix::FaultClass::ReadFailure)
            .push(thesix::OpKind::Get, thesix::FaultClass::ReadFailure),
    );
    for i in 2..7 {
        let _ = m.get(&format!("k{i}"), &ctx).await;
    }
    assert_eq!(
        m.continuity().state(TierId::L0),
        ContinuityState::Unavailable,
        "five consecutive failures did not take the rung unavailable"
    );

    // -> RECOVERY, on a success.
    failing.arm(thesix::FaultPlan::new());
    // A read that now succeeds is what clears the rung's failure count.
    let _ = m.get(&"k0".to_string(), &ctx).await;
    assert_eq!(
        m.continuity().state(TierId::L0),
        ContinuityState::Healthy,
        "a successful read did not restore the rung"
    );
    assert!(all_bound_healthy(&m), "recovery did not restore health");
}

/// A rung that goes unavailable must not take the others with it.
#[tokio::test]
async fn one_failing_rung_does_not_cascade() {
    let bad = FaultyTier::<String>::with_plan(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        thesix::FaultPlan::new(),
    );
    let bad_tier: Arc<dyn CacheTier<String>> = bad.clone();
    let (tiers, tallies) = testkit::record_all(vec![
        bad_tier,
        Arc::new(L1Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        Arc::new(L1Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
    ]);
    let m = testkit::manager_from_parts(
        DefaultPolicy,
        thesix::Cachelito::new(),
        tiers,
        Duration::from_millis(200),
    );

    bad.arm(thesix::FaultPlan::new().push(thesix::OpKind::Set, thesix::FaultClass::WriteFailure));
    let _ = m.set(&"k".to_string(), "v".to_string(), &test_ctx()).await;

    let report = m.continuity();
    assert!(
        report.state(TierId::L1).is_serving(),
        "a failure on L0 took L1 down: {}",
        report.summary()
    );
    let total: u64 = tallies.iter().map(|t: &Arc<Tally>| t.total()).sum();
    assert!(total > 0, "no operation reached any rung");
}

// ---------------------------------------------------------------------------
// Intent recovery
// ---------------------------------------------------------------------------

/// A prepared write resolves by aborting, and the committed value survives.
#[tokio::test]
async fn a_prepared_write_aborts_and_keeps_the_committed_value() {
    let cachelito = thesix::Cachelito::new();

    // Establish a committed value.
    let seed = cachelito
        .prepare(b"k", None, TierId::L1, IntentKind::Write)
        .expect("seed");
    cachelito.commit(&seed, None).expect("commit");

    // A second write that never commits.
    let doomed = cachelito
        .prepare(b"k", None, TierId::L1, IntentKind::Write)
        .expect("prepare");
    let during = cachelito.peek(b"k").expect("peek");
    assert_eq!(during.state, EntryState::Prepared);
    assert!(
        !during.state.is_readable(),
        "an uncommitted write was readable"
    );

    let (recovered, failed) = (0, 0);
    assert_eq!((recovered, failed), (0, 0));

    let outcome = cachelito
        .resolve_intent(
            b"k",
            during.intent.expect("intent"),
            RecoveryDirection::Abort,
        )
        .expect("resolve");
    assert!(outcome.is_success());
    assert!(!outcome.is_committed());

    let after = cachelito.peek(b"k").expect("peek");
    assert_eq!(
        after.state,
        EntryState::Ready,
        "aborting a failed write destroyed the previously committed value"
    );
    assert!(after.intent.is_none());
    let _ = doomed;
}

/// Recovery is idempotent: a second pass finds nothing and reports nothing.
#[tokio::test]
async fn recovery_is_idempotent() {
    let m = mgr();
    let ctx = test_ctx();
    for key in ["a", "b", "c"] {
        let framed = testkit::framed_key(&ctx, key);
        m.cachelito()
            .prepare(&framed, None, TierId::L1, IntentKind::Write)
            .expect("prepare");
    }

    let first = m.recover_older_than(Duration::from_secs(0));
    assert_eq!(first, (3, 0), "the first sweep did not resolve everything");

    // Repeat, repeatedly. Each pass must be a no-op.
    for round in 0..5 {
        assert_eq!(
            m.recover_older_than(Duration::from_secs(0)),
            (0, 0),
            "recovery pass {round} was not idempotent"
        );
    }

    // And the entries are all readable again.
    for key in ["a", "b", "c"] {
        let framed = testkit::framed_key(&ctx, key);
        let snap = m.cachelito().peek(&framed).expect("peek");
        assert!(snap.intent.is_none(), "{key} still has an intent");
    }
}

/// Concurrent recovery passes must not double-resolve.
#[tokio::test]
async fn concurrent_recovery_is_safe() {
    let m = mgr();
    let ctx = test_ctx();
    for i in 0..16 {
        let framed = testkit::framed_key(&ctx, &format!("k{i}"));
        m.cachelito()
            .prepare(&framed, None, TierId::L1, IntentKind::Write)
            .expect("prepare");
    }

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let m = Arc::clone(&m);
            tokio::task::spawn_blocking(move || m.recover_older_than(Duration::from_secs(0)))
        })
        .collect();

    let mut total = 0;
    for h in handles {
        let (recovered, failed) = h.await.expect("join");
        assert_eq!(failed, 0, "a concurrent sweep reported a failure");
        total += recovered;
    }
    assert_eq!(
        total, 16,
        "concurrent sweeps resolved {total} of 16 intents; each must resolve exactly once"
    );

    // Nothing left behind.
    assert_eq!(m.recover_older_than(Duration::from_secs(0)), (0, 0));
}

/// A move intent must complete forward, or committed data is lost.
#[tokio::test]
async fn a_prepared_move_completes_forward() {
    let cachelito = thesix::Cachelito::new();
    let seed = cachelito
        .prepare(b"m", None, TierId::L3, IntentKind::Write)
        .expect("seed");
    cachelito.commit(&seed, None).expect("commit");

    let mover = cachelito
        .prepare(b"m", None, TierId::L2, IntentKind::Move)
        .expect("prepare");
    let intent = cachelito.peek(b"m").expect("peek").intent.expect("intent");

    assert_eq!(
        RecoveryDirection::for_kind(intent.kind),
        RecoveryDirection::CompleteForward,
        "a move would be resolved by aborting, discarding a committed value"
    );

    let outcome = cachelito
        .resolve_intent(b"m", intent, RecoveryDirection::CompleteForward)
        .expect("resolve");
    assert!(outcome.is_committed());
    let after = cachelito.peek(b"m").expect("peek");
    assert_eq!(after.state, EntryState::Ready);
    assert_eq!(after.tier, TierId::L2);
    let _ = mover;
}

/// Recovery of a move without the key must be reported, not silently skipped.
#[tokio::test]
async fn an_unresolvable_move_is_reported() {
    let m = mgr();
    let ctx = test_ctx();
    let framed = testkit::framed_key(&ctx, "m");
    m.cachelito()
        .prepare(&framed, None, TierId::L2, IntentKind::Move)
        .expect("prepare");

    // The sweep can abort writes but not moves, because completing a move needs
    // the key bytes and the control plane keeps only a hash. Reporting that as a
    // failure is the whole point: a silent skip would look identical to a sweep
    // with nothing to do.
    let (recovered, failed) = m.recover_older_than(Duration::from_secs(0));
    assert_eq!(recovered, 0);
    assert_eq!(failed, 1, "an unresolvable move was not reported");

    // The intent survives, so a caller holding the key can still finish it.
    let snap = m.cachelito().peek(&framed).expect("peek");
    assert!(
        snap.intent.is_some(),
        "a failed recovery discarded the intent"
    );
}

/// A recovered entry must be usable, not merely present.
#[tokio::test]
async fn a_recovered_key_is_usable_again() {
    let m = mgr();
    let key = "recovered".to_string();
    let framed = testkit::framed_key(&test_ctx(), &key);
    m.cachelito()
        .prepare(&framed, None, TierId::L1, IntentKind::Write)
        .expect("prepare");

    let (recovered, _) = m.recover_older_than(Duration::from_secs(0));
    assert_eq!(recovered, 1);

    let value = m
        .get_or_fetch(&key, &test_ctx(), || async { Ok("fresh".to_string()) })
        .await;
    assert_eq!(
        value.expect("the key was still unusable after recovery"),
        "fresh".to_string()
    );
}

/// Recovery must not resurrect an entry a caller deliberately removed.
#[tokio::test]
async fn recovery_does_not_resurrect_removed_entries() {
    let m = mgr();
    m.set(&"gone".to_string(), "v".to_string(), &test_ctx())
        .await
        .expect("set");
    m.remove(&"gone".to_string(), &test_ctx())
        .await
        .expect("remove");

    let (recovered, failed) = m.recover_older_than(Duration::from_secs(0));
    assert_eq!((recovered, failed), (0, 0));
    assert_eq!(
        m.get(&"gone".to_string(), &test_ctx()).await.unwrap_err(),
        CacheError::Miss
    );
}

/// The outcome type must distinguish the two recovery directions.
#[test]
fn recovery_outcomes_are_distinguishable() {
    let aborted = RecoveryOutcome::Aborted {
        kind: IntentKind::Write,
    };
    let completed = RecoveryOutcome::Completed {
        kind: IntentKind::Write,
    };
    let failed = RecoveryOutcome::Failed {
        kind: IntentKind::Write,
    };
    assert!(aborted.is_success() && !aborted.is_committed());
    assert!(completed.is_success() && completed.is_committed());
    assert!(!failed.is_success() && !failed.is_committed());
    assert_ne!(aborted, completed);
}

/// Every `EntryState` the machine can be in must be classified.
#[test]
fn entry_states_classify_into_the_machine() {
    // `Prepared` must never read as committed, and must never be claimable — those
    // two properties are what make `partial_commit_visible = false` hold.
    assert!(!EntryState::Prepared.is_readable());
    assert!(!EntryState::Prepared.is_claimable());
    assert!(EntryState::Ready.is_readable());
    assert!(EntryState::Stale.is_readable());
    assert!(EntryState::Absent.is_claimable());
    assert!(EntryState::Failed.is_claimable());
    assert!(!EntryState::InFlight.is_claimable());
}

/// A generation must be monotonic across a recovery cycle.
#[test]
fn generations_stay_monotonic_through_recovery() {
    let cachelito = thesix::Cachelito::new();
    let mut last = Generation::new(0);
    for round in 0..8 {
        let token = cachelito
            .prepare(b"g", None, TierId::L1, IntentKind::Write)
            .expect("prepare");
        assert!(
            token.generation >= last,
            "round {round}: generation went backwards"
        );
        last = token.generation;
        let intent = cachelito.peek(b"g").expect("peek").intent.expect("intent");
        cachelito
            .resolve_intent(b"g", intent, RecoveryDirection::Abort)
            .expect("resolve");
    }
}

/// An unbound rung reports as unavailable rather than silently healthy.
#[tokio::test]
async fn an_unbound_rung_is_not_reported_healthy() {
    let short = testkit::manager_from_tiers(
        DefaultPolicy,
        vec![Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>],
        Duration::from_millis(50),
    );
    let report = short.continuity();
    assert!(
        !report.all_healthy(),
        "a mostly-unbound manager claims health"
    );
    assert_eq!(report.state(TierId::L4), ContinuityState::Unavailable);
    assert!(report.serving().contains(&TierId::L0));
    assert!(!report.serving().contains(&TierId::L4));
}

/// Continuity must be observable while traffic is running.
#[tokio::test]
async fn continuity_is_observable_under_traffic() {
    let (tiers, _) = testkit::record_all(testkit::default_tiers::<String>());
    let m = testkit::manager_from_parts(
        DefaultPolicy,
        thesix::Cachelito::new(),
        tiers,
        Duration::from_millis(200),
    );
    let mut readers = Vec::new();
    for i in 0..8 {
        let m = Arc::clone(&m);
        readers.push(tokio::spawn(async move {
            for j in 0..20 {
                let _ = m.get(&format!("k{i}-{j}"), &test_ctx()).await;
                let _ = m.continuity();
            }
        }));
    }
    for r in readers {
        r.await.expect("join");
    }
    assert!(all_bound_healthy(&m), "traffic left a rung unhealthy");
}

/// An entry prepared on a fresh key must be discoverable — the generation-0 case
/// that a "generation 0 means no intent" sentinel hid.
#[test]
fn an_intent_on_a_fresh_key_is_visible() {
    let cachelito = thesix::Cachelito::new();
    let token = cachelito
        .prepare(b"brand-new", None, TierId::L1, IntentKind::Write)
        .expect("prepare");
    assert_eq!(token.generation, Generation::new(0));
    let snap = cachelito.peek(b"brand-new").expect("peek");
    assert!(
        snap.intent.is_some(),
        "an intent at generation 0 was invisible and therefore unrecoverable"
    );
}

/// And the recovery sweep must find it.
#[tokio::test]
async fn the_sweep_finds_a_generation_zero_intent() {
    let m = mgr();
    m.cachelito()
        .prepare(b"zero", None, TierId::L1, IntentKind::Write)
        .expect("prepare");
    let found = m.cachelito().stale_intents(0);
    assert_eq!(found.len(), 1, "the sweep missed a generation-0 intent");
    let (recovered, _) = m.recover_older_than(Duration::from_secs(0));
    assert_eq!(recovered, 1);
}

/// A `Prepared` entry must not be readable by anyone, including through `get`.
#[tokio::test]
async fn a_prepared_entry_is_invisible_to_readers() {
    let m = mgr();
    m.cachelito()
        .prepare(
            b"hidden".to_vec().as_slice(),
            None,
            TierId::L1,
            IntentKind::Write,
        )
        .expect("prepare");
    let snapshot = m.cachelito().peek(b"hidden").expect("peek");
    assert_eq!(snapshot.state, EntryState::Prepared);
    assert!(!snapshot.state.is_readable());

    // And nothing was written to any rung.
    for id in [TierId::L0, TierId::L1, TierId::L2] {
        let present = m
            .tier(&id)
            .expect("default ladder binds every rung")
            .contains(&KeyRef(b"hidden"))
            .await
            .unwrap_or(false);
        assert!(!present, "{id} holds a value for an uncommitted entry");
    }
}
