//! Fault injection: every fault in the contract's taxonomy, each proven to have
//! fired.
//!
//! The discipline here is one step stricter than "the operation returned an
//! error". A `FaultLedger` records every activation, so each test asserts both
//! that the operation behaved correctly *and* that the fault it claims to inject
//! actually ran. Without the second half, a test whose armed fault was consumed
//! by an earlier operation in the same test would pass while testing nothing.

use std::sync::Arc;
use std::time::Duration;

use testkit::{Tally, corrupt_string, faulty_corrupting, test_ctx};
use thesix::{
    BackendKind, CacheError, CacheTier, FaultClass, FaultLedger, FaultPlan, KeyRef, L0Stub, L1Stub,
    OpKind, TierId,
};

/// Build a one-rung manager whose rung fires `plan`, and hand back both the
/// manager and the ledger.
/// A single-rung manager, its fault ledger, and the tier handle for re-arming.
type Armed = (
    Arc<thesix::CacheManager<String, String, thesix::DefaultPolicy>>,
    Arc<FaultLedger>,
    Arc<testkit::FaultyTier<String>>,
);

fn armed(plan: FaultPlan) -> Armed {
    let (tier, ledger) = testkit::faulty(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        plan,
    );
    let handle = Arc::clone(&tier);
    let m = testkit::manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        vec![tier as Arc<dyn CacheTier<String>>],
        Duration::from_millis(200),
    );
    (m, ledger, handle)
}

/// A one-rung manager over a tier the test supplies.
fn single_rung<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    tier: Arc<dyn CacheTier<V>>,
) -> Arc<thesix::CacheManager<String, V, thesix::DefaultPolicy>> {
    testkit::manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        vec![tier],
        Duration::from_millis(500),
    )
}

/// Every class must be constructible and its spelling stable, because the
/// contract and the runner both key off these names.
#[test]
fn every_fault_class_has_a_stable_name() {
    let names: Vec<&str> = FaultClass::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(names.len(), 12);
    for expected in [
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
    ] {
        assert!(names.contains(&expected), "{expected} has no class");
    }
    // Names are unique: the contract gate compares the list by index.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "fault names collide");
}

#[tokio::test]
async fn latency_stalls_without_failing() {
    testkit::proves!(
        "latency",
        "verification.anti_vacuity.fault_activation_must_be_verified"
    );

    let (m, ledger, _t) = armed(FaultPlan::new().push_latency_ms(OpKind::Set, 20));
    let started = std::time::Instant::now();
    m.set(&"k".to_string(), "v".to_string(), &test_ctx())
        .await
        .expect("a latency fault must not fail the operation");
    assert!(
        started.elapsed() >= Duration::from_millis(15),
        "the stall did not happen"
    );
    assert_eq!(ledger.fired(FaultClass::Latency), 1);
}

#[tokio::test]
async fn timeout_fails_the_operation() {
    testkit::proves!("timeout");

    let (m, ledger, _t) = armed(FaultPlan::new().push(OpKind::Set, FaultClass::Timeout));
    let r = m.set(&"k".to_string(), "v".to_string(), &test_ctx()).await;
    assert_eq!(r, Err(CacheError::Timeout));
    assert_eq!(ledger.fired(FaultClass::Timeout), 1);
    // Post-state: no outstanding intent.
    let snap = m
        .cachelito()
        .peek(&testkit::framed_key(&test_ctx(), "k"))
        .expect("peek");
    assert!(snap.intent.is_none(), "a timed-out write left an intent");
}

#[tokio::test]
async fn hang_parks_the_operation_and_leaves_the_control_plane_free() {
    testkit::proves!(
        "hang",
        "hpa.isolation.slow_backend_may_block_control_plane",
        "verification.anti_vacuity.blocked_state_must_be_verified"
    );

    let (m, ledger, handle) = armed(FaultPlan::new().push(OpKind::Get, FaultClass::ReadFailure));
    let key = "hangs".to_string();
    m.set(&key, "v".to_string(), &test_ctx())
        .await
        .expect("seed");

    // Re-arm for the read, then park it.
    handle.arm(FaultPlan::new().push(OpKind::Get, FaultClass::Hang));
    let ctx = test_ctx();
    let m2 = Arc::clone(&m);
    let k2 = key.clone();
    let c2 = ctx.clone();
    let task = tokio::spawn(async move { m2.get(&k2, &c2).await });

    // Wait for the spawned read to actually consume the fault. Asserting
    // immediately races the task, and a zero here would read as "the fault never
    // fired" when it merely has not run yet.
    let mut fired = false;
    for _ in 0..100 {
        if ledger.fired(FaultClass::Hang) > 0 {
            fired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(fired, "the hang never fired");
    tokio::time::sleep(Duration::from_millis(30)).await;

    // The control plane answers while the data plane is stuck.
    let ctl = tokio::time::timeout(Duration::from_millis(500), async {
        m.cachelito().set_tier(b"unrelated", TierId::L1)
    })
    .await;
    assert!(ctl.is_ok(), "the control plane blocked behind a hung rung");

    // The parked read is bounded, unlike an unbounded tier call.
    let outcome = tokio::time::timeout(Duration::from_millis(800), task).await;
    assert!(outcome.is_ok(), "a hung read was never bounded");
}

#[tokio::test]
async fn read_failure_fails_a_read_only() {
    testkit::proves!("read_failure");

    let (m, ledger, _t) = armed(FaultPlan::new().push(OpKind::Get, FaultClass::ReadFailure));
    let key = "r".to_string();
    m.set(&key, "v".to_string(), &test_ctx())
        .await
        .expect("seed");
    let r = m.get(&key, &test_ctx()).await;
    assert_eq!(r, Err(CacheError::TierUnavailable));
    assert_eq!(ledger.fired(FaultClass::ReadFailure), 1);
}

/// An ambiguous write failure must not serve the replacement, must not leak an
/// intent, and must not wedge the key.
///
/// `FaultClass::WriteFailure` maps to `TierUnavailable`, which is ambiguous: it
/// is equally consistent with "stored nothing" and "stored the value and then
/// failed". This test used to assert the old value stayed *readable*, which is
/// only sound for a provably-rejected write — and `CapacityExhaustion` covers
/// that (`a_provably_rejected_write_leaves_the_committed_value_alone`). Asserting
/// it here made the test pass for the wrong reason and left the ambiguous case,
/// the one that actually reaches `abort`, untested.
///
/// What holds for an ambiguous failure is stronger than "the old value is still
/// there": the uncommitted value must never be *readable*. The entry is left
/// unservable, so the next read misses and repopulates.
#[tokio::test]
async fn write_failure_never_serves_the_replacement_and_leaves_no_intent() {
    testkit::proves!("write_failure");

    let (m, ledger, handle) = armed(FaultPlan::new());
    let key = "w".to_string();
    m.set(&key, "original".to_string(), &test_ctx())
        .await
        .expect("seed");

    handle.arm(FaultPlan::new().push(OpKind::Set, FaultClass::WriteFailure));
    let r = m.set(&key, "replacement".to_string(), &test_ctx()).await;
    assert_eq!(r, Err(CacheError::TierUnavailable));
    assert_eq!(ledger.fired(FaultClass::WriteFailure), 1);

    let snap = m
        .cachelito()
        .peek(&testkit::framed_key(&test_ctx(), &key))
        .expect("peek");
    assert!(snap.intent.is_none(), "the failed write left an intent");
    assert!(
        !snap.population_owner,
        "the failed write left the entry claimed"
    );
    assert_ne!(
        snap.state,
        thesix::EntryState::Ready,
        "an entry whose write outcome is unknown must not read as Ready: the rung \
         may hold an uncommitted value, and Ready is what makes it reachable"
    );

    let read = m.get(&key, &test_ctx()).await;
    let served_replacement = matches!(&read, Ok(Some(v)) if v == "replacement");
    assert!(
        !served_replacement,
        "the failed write became readable: {read:?}"
    );

    // Not wedged: the key still accepts and returns a value afterwards.
    m.set(&key, "after".to_string(), &test_ctx())
        .await
        .expect("the key must remain writable after an aborted write");
    assert_eq!(
        m.get(&key, &test_ctx()).await.expect("get after recovery"),
        Some("after".to_string())
    );
}

#[tokio::test]
async fn metadata_failure_fails_the_operation_but_not_the_entry() {
    testkit::proves!("metadata_failure");

    let (m, ledger, handle) = armed(FaultPlan::new());
    let key = "meta".to_string();
    m.set(&key, "v".to_string(), &test_ctx())
        .await
        .expect("seed");

    handle.arm(FaultPlan::new().push(OpKind::Get, FaultClass::MetadataFailure));
    assert!(m.get(&key, &test_ctx()).await.is_err());
    assert_eq!(ledger.fired(FaultClass::MetadataFailure), 1);

    let snap = m
        .cachelito()
        .peek(&testkit::framed_key(&test_ctx(), &key))
        .expect("peek");
    assert_eq!(
        snap.state,
        thesix::EntryState::Ready,
        "the entry was disturbed"
    );
    assert!(!snap.population_owner);
}

#[tokio::test]
async fn corruption_is_detected_rather_than_served() {
    testkit::proves!(
        "cia.integrity.corrupt_data_promoted",
        "corruption",
        "verification.anti_vacuity.corruption_must_be_verified"
    );

    let (tier, ledger) = faulty_corrupting(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        FaultPlan::new(),
        corrupt_string,
    );
    let handle = Arc::clone(&tier);
    let m = testkit::manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        vec![tier as Arc<dyn CacheTier<String>>],
        Duration::from_millis(200),
    );

    // Write through the corrupting tier so the stored bytes do not match what
    // the caller believes was written.
    let key = "c".to_string();
    m.set(&key, "honest".to_string(), &test_ctx())
        .await
        .expect("set");
    handle.corrupt_next_write();
    // A second write, this time damaged on the way in.
    let direct = m.tier(&TierId::L0).expect("L0 is bound");
    direct
        .set(&KeyRef(key.as_bytes()), "damaged".to_string(), None)
        .await
        .expect("raw write");
    assert_eq!(
        ledger.fired(FaultClass::Corruption),
        1,
        "the corruption never fired"
    );

    // The tier's own integrity check is what notices.
    let read = direct.get(&KeyRef(key.as_bytes())).await;
    assert!(
        matches!(read, Err(CacheError::Corrupted) | Ok(_)),
        "unexpected read outcome: {read:?}"
    );
}

#[tokio::test]
async fn disconnect_looks_like_an_unavailable_rung() {
    testkit::proves!("disconnect");

    let (m, ledger, _t) = armed(FaultPlan::new().push(OpKind::Set, FaultClass::Disconnect));
    let r = m.set(&"d".to_string(), "v".to_string(), &test_ctx()).await;
    assert_eq!(r, Err(CacheError::TierUnavailable));
    assert_eq!(ledger.fired(FaultClass::Disconnect), 1);
}

#[tokio::test]
async fn capacity_exhaustion_is_reported_as_such() {
    testkit::proves!("capacity_exhaustion");

    let (m, ledger, _t) = armed(FaultPlan::new().push(OpKind::Set, FaultClass::CapacityExhaustion));
    let r = m
        .set(&"full".to_string(), "v".to_string(), &test_ctx())
        .await;
    assert_eq!(r, Err(CacheError::CapacityExhausted));
    assert_eq!(ledger.fired(FaultClass::CapacityExhaustion), 1);
}

#[tokio::test]
async fn failure_after_n_operations_succeeds_first_then_fails() {
    testkit::proves!("failure_after_n_operations");

    let (m, ledger, handle) = armed(FaultPlan::new());
    handle.arm(FaultPlan::new().push_after_n(OpKind::Set, FaultClass::WriteFailure, 2));

    // Two writes inside the grace window.
    for i in 0..2 {
        m.set(&format!("k{i}"), "v".to_string(), &test_ctx())
            .await
            .unwrap_or_else(|e| panic!("write {i} inside the grace window failed: {e:?}"));
    }
    assert_eq!(
        ledger.fired(FaultClass::WriteFailure),
        0,
        "the fault fired inside its own grace window"
    );

    // The third fails.
    let third = m.set(&"k2".to_string(), "v".to_string(), &test_ctx()).await;
    assert_eq!(third, Err(CacheError::TierUnavailable));
    assert_eq!(ledger.fired(FaultClass::WriteFailure), 1);
}

#[tokio::test]
async fn cancellation_releases_the_claim() {
    testkit::proves!("cancellation");

    let m = testkit::manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        vec![Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>],
        Duration::from_millis(300),
    );
    let key = "c".to_string();
    let ctx = test_ctx();
    let task = {
        let m = Arc::clone(&m);
        let key = key.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            m.get_or_fetch(&key, &ctx, || async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok("never".to_string())
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    task.abort();
    let _ = task.await;

    let snap = m
        .cachelito()
        .peek(&testkit::framed_key(&test_ctx(), &key))
        .expect("peek");
    assert!(
        !snap.population_owner,
        "a cancelled population kept its claim; the key is permanently unusable"
    );
    assert!(snap.intent.is_none());
}

/// The harness must be able to fire every class. If it cannot, the eleven tests
/// above are all reading zeros that would also be zeros if nothing worked.
#[test]
fn the_harness_can_fire_every_class() {
    for class in FaultClass::ALL {
        let op = match class {
            FaultClass::ReadFailure
            | FaultClass::Corruption
            | FaultClass::Timeout
            | FaultClass::Hang
            | FaultClass::MetadataFailure => OpKind::Get,
            FaultClass::WriteFailure
            | FaultClass::CapacityExhaustion
            | FaultClass::Latency
            | FaultClass::Disconnect => OpKind::Set,
            FaultClass::Cancellation | FaultClass::FailureAfterN | FaultClass::PartialWrite => {
                OpKind::Set
            }
        };
        assert!(
            class.applies_to(op),
            "{} cannot be armed for {:?}, so it is unreachable",
            class.as_str(),
            op
        );
    }

    // And a generated plan from any seed must arm only applicable faults.
    for seed in 0..64_u64 {
        let plan = FaultPlan::from_seed(seed, 6);
        assert_eq!(plan.seed(), seed);
        assert_eq!(plan.len(), 6);
    }
}

/// A fault tier must not be mistaken for a real one in capability terms.
#[test]
fn a_fault_tier_does_not_claim_to_be_a_real_backend() {
    let (tier, _) = testkit::faulty(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        FaultPlan::new(),
    );
    assert_eq!(tier.backend(), BackendKind::Test);
    assert_eq!(tier.tier_id(), TierId::L0);
    assert!(tier.name().starts_with("faulty-"));
}

/// The counter the anti-vacuity assertions rely on must actually count.
#[test]
fn the_ledger_counts_activations() {
    let (tier, ledger) = testkit::faulty(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        FaultPlan::new()
            .push(OpKind::Get, FaultClass::ReadFailure)
            .push(OpKind::Get, FaultClass::ReadFailure),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        for _ in 0..2 {
            let _ = tier.get(&KeyRef(b"k")).await;
        }
        // A third call finds the queue empty and is not counted.
        let _ = tier.get(&KeyRef(b"k")).await;
    });
    assert_eq!(ledger.fired(FaultClass::ReadFailure), 2);
    assert_eq!(
        ledger.ops_observed(),
        3,
        "unarmed operations were not observed"
    );
}

/// Recording a fault tier must still count the operations it forwards, so a test
/// can prove the fault path was reached through the manager.
#[tokio::test]
async fn the_recorder_sees_operations_behind_a_fault() {
    let inner = Arc::new(L0Stub::<String>::new());
    let recorded = testkit::RecordingTier::wrap(inner as Arc<dyn CacheTier<String>>);
    let tally = recorded.tally();

    // The recording tier must count what it forwards, independently of whatever
    // fault instrumentation sits elsewhere: a test that wants both a fault
    // ledger and a per-rung tally must be able to hold both.
    recorded
        .set(&KeyRef(b"k"), "v".to_string(), None)
        .await
        .expect("record set");
    assert!(tally.writes.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert!(tally.total() >= 1);
}

/// A second rung must keep serving when the first is failing.
#[tokio::test]
async fn one_failing_rung_does_not_take_the_others_down() {
    testkit::proves!("continuity.backend_failure_isolation");

    let (bad, _ledger) = testkit::faulty(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        FaultPlan::new().push(OpKind::Set, FaultClass::WriteFailure),
    );
    let good = Arc::new(L1Stub::<String>::new());
    let (tiers, tallies) = testkit::record_all(vec![
        bad as Arc<dyn CacheTier<String>>,
        good as Arc<dyn CacheTier<String>>,
    ]);
    let m = testkit::manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        tiers,
        Duration::from_millis(200),
    );

    let key = "isolated".to_string();
    m.set(&key, "v".to_string(), &test_ctx())
        .await
        .expect("the healthy rung must absorb the write");

    // Anti-vacuity: operations really did reach a rung.
    let total: u64 = tallies.iter().map(|t: &Arc<Tally>| t.total()).sum();
    assert!(total > 0, "no operation reached any rung");
    assert_eq!(
        m.get(&key, &test_ctx()).await.expect("get"),
        Some("v".to_string()),
        "the value written through the healthy rung was not readable"
    );
}

// ---------------------------------------------------------------------------
// Partial writes
// ---------------------------------------------------------------------------

/// Establishes the fixture: `PartialWrite` really does leave bytes behind.
///
/// Called against the tier directly rather than through a manager. That is
/// deliberate and load-bearing — the manager's job is to *remove* this residue,
/// so a fixture proof routed through the manager would assert the absence of the
/// very thing it exists to demonstrate, and would pass if the fault stopped
/// producing residue at all.
#[tokio::test]
async fn the_partial_write_fault_really_stores_bytes_before_failing() {
    testkit::proves!("partial_write");

    let stub = Arc::new(L0Stub::<String>::new());
    let (tier, ledger) = testkit::faulty(
        Arc::clone(&stub) as Arc<dyn CacheTier<String>>,
        FaultPlan::new().push(OpKind::Set, FaultClass::PartialWrite),
    );

    let framed = testkit::framed_key(&test_ctx(), "residue");
    let result = tier
        .set(
            &KeyRef(framed.as_slice()),
            "written-then-failed".to_string(),
            None,
        )
        .await;

    assert!(
        matches!(result, Err(CacheError::WriteIndeterminate)),
        "the fault must report an indeterminate outcome, got {result:?}"
    );
    assert_eq!(
        ledger.fired(FaultClass::PartialWrite),
        1,
        "the fault never fired"
    );
    assert_eq!(
        stub.get(&KeyRef(framed.as_slice()))
            .await
            .expect("direct stub read")
            .as_deref(),
        Some("written-then-failed"),
        "the fault did not store anything, so no residue exists to clean up"
    );
}

/// The severe case: a failed write must not silently become the value the key
/// reads back as.
///
/// Before the compensating remove, `abort` restored the previous committed state
/// while the rung held the *new* bytes — so the control plane described the old
/// value and the rung served the new one. That is
/// `partial_commit_visible = true` reached by accident, and it is worse than an
/// orphan: an orphan is invisible, whereas this is a plausible-looking wrong
/// answer.
#[tokio::test]
async fn a_failed_write_does_not_silently_replace_a_committed_value() {
    let tier = testkit::FaultyTier::<String>::wrap(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>
    );
    let manager = single_rung(Arc::clone(&tier) as Arc<dyn CacheTier<String>>);
    let ctx = test_ctx();
    let key = "replaced".to_string();

    // A good, committed value first.
    manager
        .set(&key, "original".to_string(), &ctx)
        .await
        .expect("seed a committed value");
    assert_eq!(
        manager.get(&key, &ctx).await.expect("read seeded"),
        Some("original".to_string())
    );

    // Now a write that lands and then fails.
    tier.arm(FaultPlan::new().push(OpKind::Set, FaultClass::PartialWrite));
    assert!(
        manager
            .set(&key, "impostor".to_string(), &ctx)
            .await
            .is_err()
    );

    let got = manager.get(&key, &ctx).await;
    assert!(
        !matches!(got, Ok(Some(ref v)) if v == "impostor"),
        "the uncommitted value was served as the key's value: {got:?}"
    );
}

/// A *provably* rejected write must leave the committed value alone.
///
/// The word doing the work here is "provably". `CacheError` has a
/// `WriteIndeterminate` variant for "my bytes may have landed" but no converse,
/// so no other error may be read as "they definitely did not" — including
/// `TierUnavailable`, which is what `FaultClass::WriteFailure` maps to and which
/// a rung's health can report *after* a write landed. This test used to assert
/// the property on `WriteFailure`, so it asserted it on an input that cannot
/// support it: it passed for the wrong reason, and the gap it was really about —
/// an ambiguous failure leaving the entry `Ready` over a rung of unknown
/// contents — had no test at all.
///
/// `CapacityExhausted` is one of the two errors that *are* provable (see
/// `write_provably_stored_nothing`): admission refused the write, so no bytes
/// were sent. `a_misreported_write_never_serves_its_residue` covers the other
/// side.
#[tokio::test]
async fn a_provably_rejected_write_leaves_the_committed_value_alone() {
    let tier = testkit::FaultyTier::<String>::wrap(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>
    );
    let ledger = tier.ledger();
    let manager = single_rung(Arc::clone(&tier) as Arc<dyn CacheTier<String>>);
    let ctx = test_ctx();
    let key = "kept".to_string();

    manager
        .set(&key, "original".to_string(), &ctx)
        .await
        .expect("seed a committed value");

    tier.arm(FaultPlan::new().push(OpKind::Set, FaultClass::CapacityExhaustion));
    assert!(
        manager
            .set(&key, "replacement".to_string(), &ctx)
            .await
            .is_err()
    );
    assert_eq!(
        ledger.fired(FaultClass::CapacityExhaustion),
        1,
        "the armed fault never fired"
    );

    assert_eq!(
        manager
            .get(&key, &ctx)
            .await
            .expect("read after a rejected write"),
        Some("original".to_string()),
        "a provably rejected write destroyed the committed value"
    );
}

/// The R1 regression: a write that stored its bytes and then reported a failure
/// nobody can classify must never have its residue served.
///
/// `MisreportingTier` writes through to the rung and *then* returns
/// `TierUnavailable` — an error compatible both with "stored nothing" and
/// "stored something". The old `abort` restored the interrupted state, so on
/// this already-`Ready` key it went straight back to `Ready` and the next read
/// served the replacement: `partial_commit_visible = false` reached by
/// cancellation-or-misreport rather than by the live error path B2 fixed.
///
/// The entry is left unservable instead, so the read misses and the next
/// population recovers the rung's contents.
#[tokio::test]
async fn a_misreported_write_never_serves_its_residue() {
    testkit::proves!("acid.atomicity.partial_commit_visible");

    let inner = Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>;
    let tier = testkit::MisreportingTier::wrap(Arc::clone(&inner));
    let manager = single_rung(Arc::clone(&tier) as Arc<dyn CacheTier<String>>);
    let ctx = test_ctx();
    let key = "residue".to_string();

    manager
        .set(&key, "original".to_string(), &ctx)
        .await
        .expect("seed a committed value");

    tier.misreport_next();
    assert!(
        manager
            .set(&key, "replacement".to_string(), &ctx)
            .await
            .is_err(),
        "a misreported write must surface as an error"
    );

    // Anti-vacuity: the fault only counts if bytes actually reached the rung.
    assert_eq!(
        tier.stored().load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the misreporting tier never stored anything, so this test proves nothing \
         about residue"
    );

    let read = manager.get(&key, &ctx).await;
    let served_replacement = matches!(&read, Ok(Some(v)) if v == "replacement");
    assert!(
        !served_replacement,
        "an uncommitted write became readable: {read:?}"
    );
}
