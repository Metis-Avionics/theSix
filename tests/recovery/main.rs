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
    KeyRef, L0Stub, L1Stub, RecoveryDirection, RecoveryOutcome, RecoveryReport, TierId,
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
    testkit::proves!("cia.availability.single_tier_failure_must_cascade");

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

/// An aborted write leaves the entry unservable, never `Ready`.
///
/// This is the control-plane half of the R1 regression, and it used to assert
/// the opposite. `RecoveryDirection::Abort` is what the recovery sweep applies to
/// a stale `Write` intent, and the sweep has no tier handle: it cannot know
/// whether the data step landed, and it cannot remove residue even in principle.
/// Restoring `Ready` therefore published whatever the rung happened to hold — on
/// a key that was already committed, a write that stored its bytes and then died
/// became readable again.
///
/// So the invariant is not "the committed value survives an abort". It is that
/// the entry does not read as servable, because we cannot tell what is in the
/// rung. The value is not lost: the next read misses and repopulates.
#[tokio::test]
async fn an_aborted_prepared_write_is_not_left_readable() {
    testkit::proves!(
        "acid.atomicity.cancelled_operation_may_commit_partially",
        "acid.atomicity.recovery_direction_prepare_write"
    );

    let cachelito = thesix::Cachelito::new();

    // Establish a committed value.
    let seed = cachelito
        .prepare(b"k", None, TierId::L1, IntentKind::Write)
        .expect("seed");
    cachelito.commit(&seed, None).expect("commit");
    assert_eq!(cachelito.peek(b"k").expect("peek").state, EntryState::Ready);

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
    assert_ne!(
        after.state,
        EntryState::Ready,
        "abort restored Ready over a rung whose contents are unknown"
    );
    assert!(
        !after.state.is_readable(),
        "an aborted write left the entry readable: {:?}",
        after.state
    );
    assert!(after.intent.is_none());
    assert!(!after.population_owner);
    let _ = doomed;
}

/// A *provably* unreached write does restore what it interrupted.
///
/// The counterpart to [`an_aborted_prepared_write_is_not_left_readable`], and the
/// reason `abort` and `abort_proven_clean` are separate. When the tier has
/// established that nothing was written, the previously committed value is still
/// exactly where it was, and forcing the entry unservable would turn a no-op
/// error into an unnecessary miss.
#[tokio::test]
async fn a_proven_clean_abort_restores_the_committed_value() {
    let cachelito = thesix::Cachelito::new();

    let seed = cachelito
        .prepare(b"k", None, TierId::L1, IntentKind::Write)
        .expect("seed");
    cachelito.commit(&seed, None).expect("commit");

    let generation_before = cachelito.peek(b"k").expect("peek").generation;

    let token = cachelito
        .prepare(b"k", None, TierId::L1, IntentKind::Write)
        .expect("prepare");
    cachelito
        .abort_proven_clean(&token, CacheError::CapacityExhausted)
        .expect("abort");

    let after = cachelito.peek(b"k").expect("peek");
    assert_eq!(
        after.state,
        EntryState::Ready,
        "a provably unreached write must not cost the committed value"
    );
    assert_eq!(
        after.generation, generation_before,
        "a proven-clean abort must not advance the generation: it would invalidate \
         the value it just preserved"
    );
    assert!(after.intent.is_none());
}

/// Recovery is idempotent: a second pass finds nothing and reports nothing.
#[tokio::test]
async fn recovery_is_idempotent() {
    testkit::proves!(
        "acid.atomicity.recovery_must_be_idempotent",
        "continuity.recovery.idempotent",
        "continuity.recovery.repeatable"
    );

    let m = mgr();
    let ctx = test_ctx();
    for key in ["a", "b", "c"] {
        let framed = testkit::framed_key(&ctx, key);
        m.cachelito()
            .prepare(&framed, None, TierId::L1, IntentKind::Write)
            .expect("prepare");
    }

    let first = m.recover_older_than(Duration::from_secs(0));
    assert_eq!(
        first.recovered, 3,
        "the first sweep did not resolve everything"
    );
    assert_eq!(first.failed, 0, "the first sweep reported a failure");

    // Repeat, repeatedly. Each pass must be a no-op.
    for round in 0..5 {
        assert_eq!(
            m.recover_older_than(Duration::from_secs(0)),
            RecoveryReport::default(),
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
    testkit::proves!("continuity.recovery.partial_recovery_safe");

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
        let RecoveryReport {
            recovered, failed, ..
        } = h.await.expect("join");
        assert_eq!(failed, 0, "a concurrent sweep reported a failure");
        total += recovered;
    }
    assert_eq!(
        total, 16,
        "concurrent sweeps resolved {total} of 16 intents; each must resolve exactly once"
    );

    // Nothing left behind.
    assert_eq!(
        m.recover_older_than(Duration::from_secs(0)),
        RecoveryReport::default()
    );
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

    // Without the key, a sweep has no direction it can execute. `CompleteForward`
    // is still the right instruction for a caller that *does* hold the key, which
    // is the case below — the two are not in conflict, they answer different
    // questions.
    assert_eq!(
        RecoveryDirection::for_kind(intent.kind),
        RecoveryDirection::ExternalReconciliation,
        "a sweep holding only a hash must not be told to complete a move"
    );
    assert!(
        !RecoveryDirection::for_kind(intent.kind).is_executable_here(),
        "the sweep claimed it could execute this direction"
    );
    assert!(
        !RecoveryDirection::Abort.is_success_without_the_key(),
        "aborting a prepared move would discard a committed value"
    );

    // This caller *has* the key, so complete-forward is executable and correct.
    let outcome = cachelito
        .resolve_intent(b"m", intent, RecoveryDirection::CompleteForward)
        .expect("resolve");
    assert!(outcome.is_committed());
    let after = cachelito.peek(b"m").expect("peek");
    assert_eq!(after.state, EntryState::Ready);
    assert_eq!(after.tier, TierId::L2);
    let _ = mover;
}

/// A move is reported in its own bucket, distinctly from a write.
///
/// This is the distinction the contract previously lacked. `failed` used to hold
/// both "the sweep tried and could not resolve this" and "the sweep has no
/// information to resolve this with", so a permanently stuck key was
/// indistinguishable from ordinary contention. A caller reading the report could
/// not tell whether to retry, escalate, or call an operator.
#[tokio::test]
async fn an_unresolvable_move_is_reported_separately_from_a_failure() {
    testkit::proves!("continuity.reconciliation");

    let m = mgr();
    let ctx = test_ctx();
    let framed = testkit::framed_key(&ctx, "m");
    let key_for_m = "m".to_string();
    m.cachelito()
        .prepare(&framed, None, TierId::L2, IntentKind::Move)
        .expect("prepare");

    let report = m.recover_older_than(Duration::from_secs(0));
    assert_eq!(report.recovered, 0, "a move was resolved without the key");
    assert_eq!(
        report.needs_reconciliation, 1,
        "the move was not reported as needing external reconciliation"
    );
    assert_eq!(
        report.failed, 0,
        "an unresolvable move was miscounted as a sweep failure; the two need \
         different responses from a caller"
    );

    // The intent survives untouched, so an external reconciler holding the key can
    // still finish it. A sweep that cleared it would destroy the only record that
    // a move was in flight.
    let snap = m.cachelito().peek(&framed).expect("peek");
    assert!(
        snap.intent.is_some(),
        "the sweep discarded the move intent, destroying the only evidence that a \
         move was in flight"
    );
    // The claim is released even though the move is not resolved (B15). The
    // entry moves to `Failed` rather than back to `Ready`: `acquire` accepts a
    // `Failed` entry, so the key becomes writable again, whereas a `Prepared`
    // entry with an owner is declined by both `acquire` and the write path --
    // the permanent wedge this finding describes. Restoring the pre-intent state
    // instead would point the control plane at a source rung the interrupted
    // move had already emptied.
    assert_eq!(
        snap.state,
        EntryState::Failed,
        "the released entry should be Failed, not Prepared (wedged) and not \
         Ready (which would point at an emptied source)"
    );
    assert!(
        !snap.population_owner,
        "the population claim was not released, so the key stays wedged"
    );

    // And the key is genuinely usable again: a fresh write commits through the
    // normal path.
    m.set(&key_for_m, "written-after-recovery".to_string(), &ctx)
        .await
        .expect("the key must be writable after its claim was released");
    assert_eq!(
        m.get(&key_for_m, &ctx).await.expect("read"),
        Some("written-after-recovery".to_string()),
        "the released key did not serve the value written after recovery"
    );

    // And the distinction is real, not cosmetic: a write in the same sweep is
    // resolved, a move is not, and the report says which happened.
    //
    // The move here is a *fresh* key. The first move's intent was legitimately
    // superseded by the write above -- `prepare` overwrites a retained intent --
    // which is the intended lifecycle: the evidence survives until the key is
    // next written, and not beyond.
    m.cachelito()
        .prepare(
            &testkit::framed_key(&ctx, "w"),
            None,
            TierId::L2,
            IntentKind::Write,
        )
        .expect("prepare a write");
    m.cachelito()
        .prepare(
            &testkit::framed_key(&ctx, "m2"),
            None,
            TierId::L2,
            IntentKind::Move,
        )
        .expect("prepare a second move");
    let both = m.recover_older_than(Duration::from_secs(0));
    assert_eq!(both.recovered, 1, "the write was not resolved");
    assert_eq!(
        both.needs_reconciliation, 1,
        "the move was resolved or lost"
    );
    assert_eq!(both.failed, 0);
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

    let RecoveryReport { recovered, .. } = m.recover_older_than(Duration::from_secs(0));
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

    let RecoveryReport {
        recovered, failed, ..
    } = m.recover_older_than(Duration::from_secs(0));
    assert_eq!((recovered, failed), (0, 0));
    assert_eq!(
        m.get(&"gone".to_string(), &test_ctx()).await.unwrap_err(),
        CacheError::Miss
    );
}

/// The outcome type must distinguish the two recovery directions.
#[test]
fn recovery_outcomes_are_distinguishable() {
    testkit::proves!("continuity.recovery.recovery_failure_must_be_observable");

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
    testkit::proves!("continuity.explicit_state_transitions");

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
    testkit::proves!("concurrency.testing.recovery_traffic_overlap");

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
    testkit::proves!("cia.availability.recovery_required");

    let m = mgr();
    m.cachelito()
        .prepare(b"zero", None, TierId::L1, IntentKind::Write)
        .expect("prepare");
    let found = m.cachelito().stale_intents(0);
    assert_eq!(found.len(), 1, "the sweep missed a generation-0 intent");
    let RecoveryReport { recovered, .. } = m.recover_older_than(Duration::from_secs(0));
    assert_eq!(recovered, 1);
}

/// A `Prepared` entry must not be readable by anyone, including through `get`.
#[tokio::test]
async fn a_prepared_entry_is_invisible_to_readers() {
    testkit::proves!(
        "acid.atomicity.prepare_state",
        "acid.atomicity.read_of_uncommitted_entry",
        "acid.isolation.intermediate_state_visibility"
    );

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

// ---------------------------------------------------------------------------
// Cancellation safety of the write path
// ---------------------------------------------------------------------------

/// A `set` cancelled mid-write must not leave the key wedged.
///
/// This is the exact shape of the defect: `prepare` puts the entry into
/// `Prepared` and claims population ownership, and only `commit` or `abort`
/// clears that. Before `IntentGuard` existed, dropping the future between the two
/// left `population_owner = true` with no owner, so every later operation joined
/// as a waiter against a claimant that would never arrive and failed on the
/// timeout.
///
/// The test asserts the *observable* consequence — that the key works again
/// immediately — rather than only the internal flags, because a guard that
/// cleared the flags but left the entry unreadable would pass the former.
#[tokio::test]
async fn a_cancelled_set_releases_its_commit_intent() {
    testkit::proves!("concurrency.cancel_safe");

    use testkit::HangingTier;

    let pool = thesix::MemoryPool::new(64).expect("pool");
    // L0 hangs, so the write parks in phase 2 with the intent already prepared.
    let hanging = HangingTier::wrap(Arc::new(L0Stub::new()));
    let reached = hanging.reached();
    let manager = Arc::new(thesix::CacheManager::with_timeout(
        DefaultPolicy,
        thesix::Cachelito::new(),
        thesix::TierRegistry::new(),
        vec![hanging],
        pool,
        Duration::from_millis(300),
    ));

    let ctx = test_ctx();
    let in_task = ctx.clone();
    let handle = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            manager
                .set(&"doomed".to_string(), "v".to_string(), &in_task)
                .await
        })
    };

    // Wait for the write to actually reach the stall. Without this the task might
    // be cancelled before `prepare`, and the test would pass for the wrong reason.
    tokio::time::timeout(Duration::from_secs(5), async {
        while reached.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the write never reached the hanging tier, so nothing was cancelled");

    // The intent exists and the entry is unreadable: we are genuinely mid-commit.
    let mid = manager
        .cachelito()
        .peek(testkit::framed_key(&ctx, "doomed").as_slice())
        .expect("peek");
    assert_eq!(
        mid.state,
        EntryState::Prepared,
        "the test must cancel a write that really prepared an intent"
    );
    assert!(mid.population_owner);

    handle.abort();
    let _ = handle.await;

    // The guard's Drop must have aborted the intent.
    let after = manager
        .cachelito()
        .peek(testkit::framed_key(&ctx, "doomed").as_slice())
        .expect("peek");
    assert_ne!(
        after.state,
        EntryState::Prepared,
        "the cancelled set left its intent behind"
    );
    assert!(
        !after.population_owner,
        "the cancelled set left the key claimed with no owner to release it"
    );
}

/// The consequence that matters: the population path is not wedged by a
/// cancelled write.
///
/// Asserted separately from the flag check because it is the property a consumer
/// depends on, and because it is a *different* property from the control-plane
/// flags — a guard that cleared the flags but left the entry unreadable would
/// pass the former and fail this.
///
/// The symptom is narrower than it first appears, and narrowing it down changed
/// what this test had to assert:
///
/// * `set` was never wedged. `prepare` has no claimability precondition, so a
///   second write simply re-prepares over the abandoned intent.
/// * `get` was never wedged either. It reports `Miss` on a `Prepared` entry.
/// * `get_or_fetch` was wedged, because it must *claim* the entry to populate it,
///   and `acquire` declines while `population_owner` is set. It joined as a
///   waiter against an owner that would never arrive and failed on the timeout.
///
/// So this is the only read path that observes the defect, and it is the one the
/// regression test has to drive.
#[tokio::test]
async fn a_cancelled_write_does_not_wedge_the_population_path() {
    testkit::proves!("concurrency.testing.cancellation_races");

    use thesix::fault::{FaultClass, FaultPlan, OpKind};

    let faulty = FaultyTier::wrap(Arc::new(L0Stub::new()));
    let ledger = faulty.ledger();
    // One-shot: `take_for` removes the fault once claimed, so this delays the
    // first write and leaves every later operation alone.
    faulty.arm(FaultPlan::new().push_latency_ms(OpKind::Set, 5_000));

    let pool = thesix::MemoryPool::new(64).expect("pool");
    let manager = Arc::new(thesix::CacheManager::with_timeout(
        DefaultPolicy,
        thesix::Cachelito::new(),
        thesix::TierRegistry::new(),
        vec![faulty],
        pool,
        Duration::from_millis(600),
    ));

    let ctx = test_ctx();
    let in_task = ctx.clone();
    let handle = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            manager
                .set(&"wedged".to_string(), "v".to_string(), &in_task)
                .await
        })
    };

    // Anti-vacuity: the stall must have actually fired, or the cancellation below
    // proves nothing.
    tokio::time::timeout(Duration::from_secs(5), async {
        while ledger.fired(FaultClass::Latency) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the armed latency never fired, so no write was cancelled");

    handle.abort();
    let _ = handle.await;

    // With the claim abandoned this joined as a waiter and burned the full 600ms
    // before failing. With the guard, it claims immediately and populates.
    let started = std::time::Instant::now();
    let got = manager
        .get_or_fetch(&"wedged".to_string(), &ctx, || async {
            Ok("fetched".to_string())
        })
        .await;
    let elapsed = started.elapsed();
    assert_eq!(
        got.as_deref(),
        Ok("fetched"),
        "the population path could not claim a key whose write was cancelled"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "population waited {elapsed:?}; it joined the abandoned claim instead of taking it"
    );
}
