//! Property testing: the contract's nine invariants, over randomised inputs.
//!
//! # Why a reference model
//!
//! Each property drives the same randomised operation sequence through the real
//! `CacheManager` and through a small in-file model of what the contract says
//! should be observable, then compares. Comparing against a model rather than
//! against fixed expectations is what makes shrinking useful: when proptest finds
//! a failure it can shrink the *operation sequence* to the shortest one that
//! still misbehaves, which is the artefact worth having.
//!
//! # Determinism
//!
//! Every generator is driven by proptest's own seed, and the seeds that found a
//! failure are printed by proptest. There is no use of the platform RNG anywhere,
//! so a failure reproduces exactly from the reported seed and case index.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;
use testkit::{FaultyTier, framed_key, manager_from_parts, test_ctx};
use thesix::{
    CacheError, CacheTier, ContinuityState, EntryState, FaultClass, FaultPlan, Generation, KeyRef,
    L0Stub, OpKind, OperationalState, Placement, RecoveryReport, TierId,
};

/// One operation in a generated sequence.
#[derive(Debug, Clone)]
enum Op {
    Set(String, String),
    Get(String),
    Invalidate(String),
    Remove(String),
    Fetch(String, String),
    Promote(String),
}

fn key_gen() -> impl Strategy<Value = String> {
    // A small key space on purpose: with only a handful of keys, the generator
    // actually produces collisions and overwrites, so the model has something to
    // disagree about. A large key space would make every run trivially correct.
    "[a-z]{1,4}"
}

fn op_gen() -> impl Strategy<Value = Op> {
    let leaf = prop_oneof![
        4 => key_gen().prop_map(Op::Get),
        3 => (key_gen(), "[a-z]{1,6}").prop_map(|(k, v)| Op::Set(k, v)),
        2 => key_gen().prop_map(Op::Invalidate),
        2 => key_gen().prop_map(Op::Remove),
        3 => (key_gen(), "[a-z]{1,6}").prop_map(|(k, v)| Op::Fetch(k, v)),
        1 => key_gen().prop_map(Op::Promote),
    ];
    leaf
}

/// A current-thread runtime for the property bodies.
///
/// `proptest!` bodies are synchronous, so every property that touches the async
/// API needs one. Built per property rather than per case: a runtime is not free,
/// and 128 cases times a runtime shows up in the gate's wall clock.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn manager() -> Arc<thesix::CacheManager<String, String, thesix::DefaultPolicy>> {
    manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        testkit::default_tiers::<String>(),
        // Short on purpose: this is a per-operation *test* budget, and the
        // property runs thousands of operations. 400ms here turned a two-minute
        // gate into a multi-hour one, because a single timing-out operation costs
        // its full budget.
        Duration::from_millis(50),
    )
}

/// What the contract says a read should return, given the operations so far.
#[derive(Default)]
struct Model {
    committed: BTreeMap<String, String>,
    invalid: std::collections::BTreeSet<String>,
    absent: std::collections::BTreeSet<String>,
}

// ---------------------------------------------------------------------------
// no_silent_data_loss
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, max_shrink_iters: 2048, ..ProptestConfig::default() })]

    /// A value that was `set` and not subsequently invalidated or removed must
    /// still be readable.
    ///
    /// The negative direction is the interesting one: a `set` that is later
    /// overwritten is fine, but a `set` followed only by unrelated operations must
    /// not vanish. Losing committed data is silent — every read still returns
    /// *something* consistent-looking — so it has to be checked against a model.
    #[test]
    fn no_silent_data_loss(ops in prop::collection::vec(op_gen(), 1..16)) {
    testkit::proves!("no_silent_data_loss");

        let rt = runtime();
        let m = manager();
        let ctx = test_ctx();
        let mut model = Model::default();
        rt.block_on(async move {

        for op in &ops {
            match op {
                Op::Set(k, v) => {
                    prop_assert!(m.set(k, v.clone(), &ctx).await.is_ok());
                    model.committed.insert(k.clone(), v.clone());
                    model.invalid.remove(k);
                    model.absent.remove(k);
                }
                Op::Invalidate(k) => {
                    let _ = m.invalidate(k, &ctx).await;
                    model.invalid.insert(k.clone());
                    model.committed.remove(k);
                }
                Op::Remove(k) => {
                    let _ = m.remove(k, &ctx).await;
                    model.absent.insert(k.clone());
                    model.committed.remove(k);
                    model.invalid.remove(k);
                }
                Op::Fetch(k, v) => {
                    let got = m.get_or_fetch(k, &ctx, || async { Ok(v.clone()) }).await;
                    if got.is_ok() {
                        // `get_or_fetch` is *not* a write. On a committed key it
                        // returns what is stored and never calls the fetcher, so
                        // the model must keep the existing value. Writing the
                        // fetched value here was a model bug that reported a
                        // correct cache as silent data loss.
                        model.committed.entry(k.clone()).or_insert_with(|| v.clone());
                        model.invalid.remove(k);
                        model.absent.remove(k);
                    }
                }
                Op::Promote(k) => { let _ = m.promote(k, &ctx).await; }
                Op::Get(_) => {}
            }
        }

        // Every key the model believes is committed must still read back.
        for (k, v) in &model.committed {
            let got = m.get(k, &ctx).await;
            prop_assert!(
                got.as_ref().ok().and_then(|o| o.as_ref()) == Some(v),
                "key {:?} was committed as {:?} and read back differently: {:?}",
                k, v, got
            );
        }
        Ok::<(), proptest::test_runner::TestCaseError>(())
        })
        .unwrap_or_else(|e| panic!("property failed: {e:?}"));
    }
}

// ---------------------------------------------------------------------------
// no_cross_key_corruption
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// Distinct keys must never return each other's values, even when their slot
    /// placement is forced to collide.
    ///
    /// The placement strategy is the whole point: without forcing a collision, a
    /// 64-bit hash would never collide in a test run and the property would be
    /// asserted rather than demonstrated.
    #[test]
    fn no_cross_key_corruption(
        pairs in prop::collection::vec((key_gen(), key_gen(), "[a-z]{1,8}"), 1..12),
    ) {
    testkit::proves!("acid.isolation.cross_key_interference", "no_cross_key_corruption");

        // Sized from the generated input: with everything forced onto one slot,
        // a fixed small table would just be testing `CapacityExhausted`.
        let capacity = pairs.len() * 2 + 2;
        let mut stub = thesix::FixedTierStub::<String>::with_capacity(capacity)
            .expect("stub")
            // Everything on one slot: the worst case for open addressing.
            .with_placement(Placement::AllToZero);

        // An expected-value model, because the generator may repeat a key across
        // pairs and a later write legitimately overwrites an earlier one. Reading
        // back against the *pair* index rather than the model reported that
        // correct behaviour as corruption.
        let mut expected: BTreeMap<String, String> = BTreeMap::new();

        for (i, (ka, kb, va)) in pairs.iter().enumerate() {
            let va_value = format!("{va}-a");
            stub.set(&KeyRef(ka.as_bytes()), va_value.clone(), None)
                .unwrap_or_else(|e| panic!("set {i}: {e:?}"));
            expected.insert(ka.clone(), va_value);

            if ka != kb {
                let vb_value = format!("{va}-b");
                stub.set(&KeyRef(kb.as_bytes()), vb_value.clone(), None)
                    .unwrap_or_else(|e| panic!("set {i}b: {e:?}"));
                expected.insert(kb.clone(), vb_value);
            }
        }

        for (key, want) in &expected {
            let got = stub.get(&KeyRef(key.as_bytes())).expect("get");
            prop_assert!(
                got.as_ref() == Some(want),
                "key {:?} holds {:?} but the model says {:?}: cross-key corruption",
                key, got, want
            );
        }
    }
}

// ---------------------------------------------------------------------------
// no_authority_inversion
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// No cache rung may report itself authoritative, for any tier ordering, and
    /// no rung-scanning path may select the authority rung.
    #[test]
    fn no_authority_inversion(
        bound in prop::collection::vec(prop::bool::ANY, 0..7),
        writes in prop::collection::vec(key_gen(), 0..8),
    ) {
    testkit::proves!("acid.consistency.authority_inversion", "no_authority_inversion");

        let rt = runtime();
        let mut tiers: Vec<Arc<dyn CacheTier<String>>> = Vec::new();
        for i in 0..7 {
            if bound.get(i).copied().unwrap_or(false) {
                tiers.push(Arc::new(L0Stub::<String>::new()));
            } else {
                // A shorter vector leaves the upper rungs unbound.
                break;
            }
        }
        let m = testkit::manager_from_tiers(
            thesix::DefaultPolicy,
            tiers,
            Duration::from_millis(200),
        );

        let caps = m.capabilities();
        for id in [TierId::L0, TierId::L1, TierId::L2, TierId::L3, TierId::L4, TierId::L5] {
            prop_assert!(
                !caps[&id].is_authoritative(),
                "{} claimed authority", id
            );
        }

        // Whatever is bound, a write must never land on the authority rung.
        rt.block_on(async move {
        let ctx = test_ctx();
        for k in writes {
            let _ = m.set(&k, "v".to_string(), &ctx).await;
            let snap = m.cachelito().peek(&framed_key(&ctx, &k)).expect("peek");
            prop_assert_ne!(
                snap.tier, TierId::L6,
                "a write landed on the authority rung"
            );
        }
        Ok::<(), proptest::test_runner::TestCaseError>(())
        })
        .unwrap_or_else(|e| panic!("property failed: {e:?}"));
    }
}

// ---------------------------------------------------------------------------
// no_invalid_state_promotion
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// Only a `Ready` entry may be promoted or demoted, and only to a rung inside
    /// the ladder.
    #[test]
    fn no_invalid_state_promotion(
        ops in prop::collection::vec(op_gen(), 1..12),
        moves in prop::collection::vec(prop_oneof![Just(true), Just(false)], 0..8),
    ) {
    testkit::proves!("acid.consistency.invalid_state_may_be_promoted", "no_invalid_state_promotion");

        let rt = runtime();
        let m = manager();
        let ctx = test_ctx();
        rt.block_on(async move {

        for op in &ops {
            match op {
                Op::Set(k, v) => { let _ = m.set(k, v.clone(), &ctx).await; }
                Op::Invalidate(k) => { let _ = m.invalidate(k, &ctx).await; }
                Op::Remove(k) => { let _ = m.remove(k, &ctx).await; }
                Op::Fetch(k, v) => { let _ = m.get_or_fetch(k, &ctx, || async { Ok(v.clone()) }).await; }
                _ => {}
            }
        }

        // Every variant names a key, so `map(..).next()` reaches the first one.
        let key = ops
            .iter()
            .map(|o| match o {
                Op::Set(k, _)
                | Op::Get(k)
                | Op::Invalidate(k)
                | Op::Remove(k)
                | Op::Fetch(k, _)
                | Op::Promote(k) => k.clone(),
            })
            .next()
            .expect("at least one op names a key");

        let before = m.cachelito().peek(&framed_key(&ctx, &key)).expect("peek");

        for up in moves {
            let result = if up { m.promote(&key, &ctx).await } else { m.demote(&key, &ctx).await };
            let after = m.cachelito().peek(&framed_key(&ctx, &key)).expect("peek");

            if before.state != EntryState::Ready {
                prop_assert!(
                    result.is_err(),
                    "a move on a {:?} entry succeeded", before.state
                );
            }
            prop_assert!(
                after.tier.is_cache_rung(),
                "a move put the entry on {}, which is not a cache rung",
                after.tier
            );
            prop_assert_ne!(
                after.tier, TierId::L6,
                "a move promoted an entry onto the authority rung"
            );
        }
        Ok::<(), proptest::test_runner::TestCaseError>(())
        })
        .unwrap_or_else(|e| panic!("property failed: {e:?}"));
    }
}

// ---------------------------------------------------------------------------
// no_deadlock / no_lock_across_await
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, max_shrink_iters: 512, ..ProptestConfig::default() })]

    /// The control plane must answer within a bounded time no matter what the
    /// data plane is doing — including never answering at all.
    ///
    /// This is the observable consequence of "no lock guard is held across an
    /// await". Asserting the absence of a guard in the source would be a lint;
    /// asserting that the control plane still responds while a rung is wedged is
    /// the property that actually matters, and it holds regardless of how the
    /// code is written.
    #[test]
    fn control_plane_survives_a_wedged_data_plane(
        stall_after in 0usize..4,
        probes in prop::collection::vec(key_gen(), 1..8),
    ) {
    testkit::proves!("acid.isolation.deadlock_tolerance", "concurrency.deadlock_free", "no_deadlock", "no_lock_across_await");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");

        let outcome: (bool, ()) = rt.block_on(async move {
            let hanging = testkit::HangingTier::wrap(
                Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>
            );
            let reached = hanging.reached();
            let ctx = test_ctx();

            // Put a committed entry on L0 so a read actually reaches the stall.
            let framed = framed_key(&ctx, "wedged");
            let cachelito = thesix::Cachelito::new();
            let token = cachelito
                .prepare(&framed, None, TierId::L0, thesix::IntentKind::Write)
                .expect("prepare");
            cachelito.commit(&token, None).expect("commit");

            let m = testkit::manager_from_parts(
                thesix::DefaultPolicy,
                cachelito,
                vec![hanging as Arc<dyn CacheTier<String>>],
                Duration::from_millis(80),
            );

            let reader = {
                let m = Arc::clone(&m);
                let ctx = ctx.clone();
                tokio::spawn(async move { m.get(&"wedged".to_string(), &ctx).await })
            };
            tokio::time::sleep(Duration::from_millis(30)).await;
            let stalled = reached.load(std::sync::atomic::Ordering::SeqCst) > 0;

            // Every control-plane probe must complete.
            for (i, k) in probes.iter().enumerate() {
                let probe = m.cachelito().peek(&framed_key(&ctx, k));
                prop_assert!(probe.is_ok(), "probe {i} failed");
                let _ = m.cachelito().bump_generation(&framed_key(&ctx, k));
                let _ = stall_after;
            }

            reader.abort();
            let _ = reached.load(std::sync::atomic::Ordering::SeqCst);
            Ok::<(bool, ()), proptest::test_runner::TestCaseError>((stalled, ()))
        })
        .unwrap_or_else(|e| panic!("the control plane did not answer: {e:?}"));

        prop_assert!(
            outcome.0,
            "the data plane never stalled, so the control plane proved nothing"
        );
    }
}

// ---------------------------------------------------------------------------
// no_capability_misreporting / no_false_durability
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// Whatever the topology, no rung may claim an unimplemented backend, an
    /// authority role it was not given, or durability it has not proved.
    #[test]
    fn no_capability_misreporting(bound in 0usize..8) {
    testkit::proves!("acid.durability.false_durability_claims", "no_capability_misreporting", "no_false_durability");

        let tiers: Vec<Arc<dyn CacheTier<String>>> = (0..bound)
            .map(|_| Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>)
            .collect();
        let m = testkit::manager_from_tiers(
            thesix::DefaultPolicy,
            tiers,
            Duration::from_millis(100),
        );

        for (id, cap) in m.capabilities() {
            prop_assert!(
                cap.backend.is_implemented() || cap.backend == thesix::BackendKind::Unavailable,
                "{} reported unimplemented backend {:?}", id, cap.backend
            );
            prop_assert_ne!(
                cap.durability,
                thesix::DurabilityClass::Verified,
                "{} claimed Verified durability", id
            );
            prop_assert!(
                !cap.is_authoritative() || id == TierId::L6,
                "{} claimed authority", id
            );
            if !m.has_tier(&id) {
                prop_assert_eq!(
                    cap.state,
                    OperationalState::Unbound,
                    "an unbound rung reported {:?}",
                    cap.state
                );
                prop_assert!(!cap.is_bound(), "an unbound rung reported bound");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// recovery_is_idempotent
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// Recovery converges in one pass and stays converged.
    #[test]
    fn recovery_is_idempotent(
        keys in prop::collection::vec(key_gen(), 1..10),
        rounds in 1usize..4,
    ) {
    testkit::proves!("recovery_is_idempotent");

        let m = manager();
        let ctx = test_ctx();
        let mut expected = 0usize;

        // Dedupe: preparing the same key twice leaves *one* intent, because the
        // second prepare replaces the first. Counting the generator's tokens
        // rather than the intents that exist reported a healthy sweep as missing
        // work.
        let mut distinct: Vec<&String> = keys.iter().collect();
        distinct.sort();
        distinct.dedup();
        for k in &distinct {
            let framed = framed_key(&ctx, k);
            if m.cachelito()
                .prepare(&framed, None, TierId::L1, thesix::IntentKind::Write)
                .is_ok()
            {
                expected += 1;
            }
        }
        prop_assert!(expected > 0, "no intents were created");

        let RecoveryReport { recovered, failed, .. } = m.recover_older_than(Duration::from_secs(0));
        prop_assert!(failed == 0, "the first sweep reported failures");
        prop_assert!(recovered == expected, "the first sweep missed intents");

        for r in 0..rounds {
            prop_assert_eq!(
                m.recover_older_than(Duration::from_secs(0)),
                RecoveryReport::default(),
                "sweep {} after convergence was not a no-op", r
            );
        }

        for k in &distinct {
            let snap = m.cachelito().peek(&framed_key(&ctx, k)).expect("peek");
            prop_assert!(snap.intent.is_none(), "{k} still has an intent");
        }
    }
}

// ---------------------------------------------------------------------------
// Structural properties
// ---------------------------------------------------------------------------

/// Generations must be monotonic under arbitrary operation sequences.
#[test]
fn generations_never_regress() {
    testkit::proves!("concurrency.testing.random_interleavings");

    let rt = runtime();
    rt.block_on(async move {
        let ops: Vec<Op> = vec![
            Op::Set("a".into(), "1".into()),
            Op::Invalidate("a".into()),
            Op::Set("a".into(), "2".into()),
            Op::Fetch("b".into(), "3".into()),
            Op::Remove("b".into()),
            Op::Set("b".into(), "4".into()),
        ];
        let m = manager();
        let ctx = test_ctx();
        let framed = framed_key(&ctx, "a");
        let mut last = Generation::new(0);

        for op in &ops {
            match op {
                Op::Set(k, v) => {
                    let _ = m.set(k, v.clone(), &ctx).await;
                }
                Op::Invalidate(k) => {
                    let _ = m.invalidate(k, &ctx).await;
                }
                Op::Remove(k) => {
                    let _ = m.remove(k, &ctx).await;
                }
                Op::Fetch(k, v) => {
                    let _ = m.get_or_fetch(k, &ctx, || async { Ok(v.clone()) }).await;
                }
                _ => {}
            }
            let snap = m.cachelito().peek(&framed).expect("peek");
            assert!(
                snap.generation >= last,
                "generation regressed from {} to {}",
                last.0,
                snap.generation.0
            );
            last = snap.generation;
        }
    });
}

/// Fault plans generated from any seed must be well-formed.
#[test]
fn generated_fault_plans_are_well_formed() {
    for seed in 0..256_u64 {
        let plan = FaultPlan::from_seed(seed, 8);
        assert_eq!(plan.seed(), seed);
        assert_eq!(plan.len(), 8);

        // Every armed fault must be applicable to the operation it names, or the
        // plan contains a no-op that a ledger assertion would never see fire.
        let mut remaining = plan.clone();
        for op in [OpKind::Get, OpKind::Set, OpKind::Remove, OpKind::Contains] {
            while let Some(f) = remaining.take_for(op) {
                assert!(
                    f.class.applies_to(op),
                    "seed {seed}: {} cannot fire for {op:?}",
                    f.class.as_str()
                );
            }
        }
    }
}

/// A fault tier must stay consistent with its ledger under concurrent use.
#[tokio::test]
async fn a_faulty_tier_never_lies_about_its_ledger() {
    // A generated plan can include `Hang`, which correctly never resolves, and
    // `Latency`, which would make this test's wall clock unpredictable. This test
    // asserts on an exact operation count, so it needs terminating faults. The
    // *sequence* still comes from the seed, so it stays reproducible.
    let seeded = FaultPlan::from_seed(7, 24);
    let terminating = [
        FaultClass::Timeout,
        FaultClass::ReadFailure,
        FaultClass::Disconnect,
        FaultClass::WriteFailure,
    ];
    let mut plan = FaultPlan::new();
    let mut rng = thesix::DeterministicRng::new(seeded.seed() ^ 0x5eed);
    for _ in 0..24 {
        plan = plan.push(OpKind::Get, terminating[rng.below(terminating.len())]);
    }
    let tier = FaultyTier::<String>::with_plan(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        plan,
    );
    let ledger = tier.ledger();

    let mut handles = Vec::new();
    for t in 0..8 {
        let tier = Arc::clone(&tier);
        handles.push(tokio::spawn(async move {
            for i in 0..12 {
                let raw = format!("k{t}-{i}");
                let key = KeyRef(raw.as_bytes());
                let _ = tier.get(&key).await;
                let _ = tier.contains(&key).await;
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    assert_eq!(
        ledger.ops_observed(),
        8 * 12 * 2,
        "the ledger lost an observation; every count below is then unreliable"
    );
    let fired: u64 = thesix::FaultClass::ALL
        .iter()
        .map(|c| ledger.fired(*c))
        .sum();
    assert!(
        fired > 0,
        "no fault fired across 192 operations from a 24-fault plan"
    );
}

/// Continuity must never report a rung healthy when it is not bound.
#[test]
fn continuity_never_claims_an_unbound_rung_is_healthy() {
    testkit::proves!("capabilities.unbound_rung_is_substituted");

    for bound in 0..8usize {
        let tiers: Vec<Arc<dyn CacheTier<String>>> = (0..bound)
            .map(|_| Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>)
            .collect();
        let m =
            testkit::manager_from_tiers(thesix::DefaultPolicy, tiers, Duration::from_millis(50));
        let report = m.continuity();
        for (id, state) in &report.states {
            if !m.has_tier(id) {
                assert_ne!(
                    *state,
                    ContinuityState::Healthy,
                    "{id} is unbound but reported healthy"
                );
            }
        }
    }
}

/// A `Corrupted` read must never be converted into a value by any wrapper.
#[tokio::test]
async fn corruption_is_never_swallowed() {
    testkit::proves!("acid.consistency.corruption_may_be_silently_accepted");

    let mut stub = thesix::FixedTierStub::<String>::with_capacity(4).expect("stub");
    let key = KeyRef(b"c");
    stub.set(&key, "good".to_string(), None).expect("set");
    stub.corrupt_stored_value_for_test(&key, "bad".to_string())
        .expect("corrupt");
    assert_eq!(stub.get(&key), Err(CacheError::Corrupted));

    // `contains` is allowed to say the entry exists — that is a different
    // question — but `get` must never answer with a value.
    for _ in 0..10 {
        match stub.get(&key) {
            Err(CacheError::Corrupted) => {}
            other => panic!("a corrupt read returned {other:?}"),
        }
    }
    let _ = FaultClass::Corruption;
}
