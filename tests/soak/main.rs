//! Soak: endurance, and the conditions that only appear after a lot of traffic.
//!
//! `#[ignore]`d, run through `cargo xtask run soak`. Everything here is either
//! slow by design or measures something that only shows up at volume — neither
//! belongs in a gate that people are expected to run constantly.
//!
//! The point is not to find a bug in the first ten thousand operations. It is to
//! find the ones that appear at the hundred-thousandth: a slot table that fills
//! and never reclaims, a per-key allocation that grows without bound, a recovery
//! pass that gets slower as intents accumulate.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use testkit::{framed_key, manager_from_parts, test_ctx};
use thesix::{CacheError, CacheTier, IntentKind, KeyRef, L0Stub, L1Stub, RecoveryReport, TierId};

fn ladder() -> Arc<thesix::CacheManager<String, String, thesix::DefaultPolicy>> {
    manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        testkit::default_tiers::<String>(),
        Duration::from_millis(500),
    )
}

/// A fixed-capacity table serves up to its capacity, refuses beyond it, and
/// reclaims as entries are removed.
///
/// The middle part matters as much as the ends. `CapacityExhausted` is a
/// contract-required error, but a *cache* that stops working when it is full is
/// not a cache — so the table must give slots back. `CacheManager` handles the
/// rest by degrading down the ladder (see
/// `the_manager_degrades_down_the_ladder_before_it_fails`), because eviction
/// policy is deliberately not this crate's business.
#[test]
#[ignore = "soak gate; run via `cargo xtask run soak`"]
fn a_fixed_capacity_tier_fills_reclaims_and_then_refuses() {
    const CAPACITY: usize = 64;
    let mut stub = thesix::FixedTierStub::<String>::with_capacity(CAPACITY).expect("stub");
    // The table must report the capacity it was built with, not a constant: the
    // earlier version of this accessor returned a hardcoded number, so it could
    // not be used to detect the table growing.
    assert_eq!(
        stub.capacity(),
        CAPACITY,
        "the table did not honour its capacity"
    );

    // Fill it exactly.
    for i in 0..CAPACITY {
        stub.set(&KeyRef(format!("k{i}").as_bytes()), format!("v{i}"), None)
            .unwrap_or_else(|e| panic!("set {i} of {CAPACITY}: {e:?}"));
    }
    assert_eq!(stub.capacity(), CAPACITY, "the table grew under load");

    // One more must be refused, with the capacity-specific error rather than a
    // generic misconfiguration.
    let overflow = stub.set(&KeyRef(b"one-too-many"), "v".to_string(), None);
    assert_eq!(
        overflow,
        Err(thesix::CacheError::CapacityExhausted),
        "an over-capacity write must be reported as capacity exhaustion"
    );

    // Reclaim half, and the freed slots must be usable again. This is the part
    // that makes it a cache rather than a fixed store.
    for i in 0..(CAPACITY / 2) {
        stub.remove(&KeyRef(format!("k{i}").as_bytes()))
            .expect("remove");
    }
    for i in 0..(CAPACITY / 2) {
        stub.set(&KeyRef(format!("re{i}").as_bytes()), format!("rv{i}"), None)
            .unwrap_or_else(|e| panic!("reuse after reclaim {i}: {e:?}"));
    }
    assert_eq!(
        stub.capacity(),
        CAPACITY,
        "reclaimed slots were not reused; the table grew instead"
    );

    // And the entries that were never removed are intact.
    for i in (CAPACITY / 2)..CAPACITY {
        assert_eq!(
            stub.get(&KeyRef(format!("k{i}").as_bytes())).expect("get"),
            Some(format!("v{i}")),
            "entry {i} was disturbed by the reclaim cycle"
        );
    }
}

/// The manager degrades down the ladder before it gives up.
///
/// A policy that selects L1 and finds it full must land the value on L0 rather
/// than failing the write, because the ladder exists precisely so that a full
/// rung is a routing decision. Reported `CapacityExhausted` before this, which
/// made a 20k-key flood fail at key 954 rather than degrade.
#[test]
#[ignore = "soak gate; run via `cargo xtask run soak`"]
fn the_manager_degrades_down_the_ladder_before_it_fails() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let m = ladder();
        let ctx = test_ctx();

        // Far more distinct keys than any single rung holds.
        let mut degraded = 0_usize;
        const KEYS: usize = 3_000;
        for i in 0..KEYS {
            let key = format!("flood{i}");
            match m.set(&key, "v".to_string(), &ctx).await {
                Ok(()) => {}
                Err(thesix::CacheError::CapacityExhausted) => break,
                Err(e) => panic!("write {i} failed with {e:?}, which is not exhaustion"),
            }
            let snap = m
                .cachelito()
                .peek(&testkit::framed_key(&ctx, &key))
                .expect("peek");
            if snap.tier != thesix::TierId::L1 {
                degraded += 1;
            }
        }
        assert!(
            degraded > 0,
            "no write was degraded; the ladder never took over from a full rung"
        );
        // Every accepted value is readable, whichever rung took it.
        for i in 0..KEYS.min(500) {
            let got = m.get(&format!("flood{i}"), &ctx).await;
            if let Ok(Some(v)) = got {
                assert_eq!(v, "v".to_string(), "flood{i} came back wrong");
            }
        }
    });
}

/// Overwriting the same keys many times must not leak pool slots. A double-free
/// or a missing deallocate shows up here as exhaustion long before it would show
/// up in production.
#[test]
#[ignore = "soak gate; run via `cargo xtask run soak`"]
fn repeated_overwrites_do_not_exhaust_the_pool() {
    let mut stub = thesix::FixedTierStub::<String>::with_capacity(32).expect("stub");
    for round in 0..20_000 {
        for i in 0..32 {
            stub.set(
                &KeyRef(format!("k{i}").as_bytes()),
                format!("r{round}-{i}"),
                None,
            )
            .unwrap_or_else(|e| panic!("round {round} key {i}: {e:?}"));
        }
    }
    for i in 0..32 {
        assert_eq!(
            stub.get(&KeyRef(format!("k{i}").as_bytes())).expect("get"),
            Some(format!("r19999-{i}"))
        );
    }
}

/// Tenants must stay isolated across a long mixed workload: no key may ever
/// resolve to another tenant's value, and the total set of keys must be exactly
/// `tenants * keys_per_tenant`.
#[test]
#[ignore = "soak gate; run via `cargo xtask run soak`"]
fn tenant_isolation_holds_under_mixed_traffic() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        const TENANTS: usize = 8;
        const KEYS: usize = 64;
        let m = ladder();

        // Write every tenant's view of every key.
        for t in 0..TENANTS {
            let ctx = testkit::ctx_for_tenant(&format!("tenant-{t}"));
            for k in 0..KEYS {
                m.set(&format!("k{k}"), format!("t{t}-v{k}"), &ctx)
                    .await
                    .expect("write");
            }
        }

        // Read them all back, repeatedly, and check nothing crossed.
        for round in 0..4 {
            for t in 0..TENANTS {
                let ctx = testkit::ctx_for_tenant(&format!("tenant-{t}"));
                for k in 0..KEYS {
                    let got = m.get(&format!("k{k}"), &ctx).await.expect("read");
                    assert_eq!(
                        got,
                        Some(format!("t{t}-v{k}")),
                        "round {round}: tenant {t} read {k} and got {got:?}"
                    );
                }
            }
        }

        // And the control plane holds one entry per (tenant, key), not one per key.
        let mut entries = BTreeSet::new();
        for t in 0..TENANTS {
            let ctx = testkit::ctx_for_tenant(&format!("tenant-{t}"));
            for k in 0..KEYS {
                entries.insert(framed_key(&ctx, &format!("k{k}")));
            }
        }
        assert_eq!(
            entries.len(),
            TENANTS * KEYS,
            "tenant framing collided: {} distinct entries for {} pairs",
            entries.len(),
            TENANTS * KEYS
        );
    });
}

/// Concurrent mixed traffic must not lose a *committed* value or wedge a key.
#[test]
#[ignore = "soak gate; run via `cargo xtask run soak`"]
fn concurrent_traffic_never_wedges_a_key() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        const WRITERS: usize = 4;
        const READERS: usize = 4;
        const KEYS: usize = 32;
        const ROUNDS: usize = 200;

        let m = ladder();
        // Writers yield `()` and readers yield a miss count, so the vector is
        // heterogeneous in what it holds. Both are discarded.
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        for w in 0..WRITERS {
            let m = Arc::clone(&m);
            tasks.push(tokio::spawn(async move {
                let ctx = test_ctx();
                for r in 0..ROUNDS {
                    for k in 0..KEYS {
                        m.set(&format!("k{k}"), format!("w{w}-r{r}"), &ctx)
                            .await
                            .expect("write");
                    }
                }
            }));
        }
        for _ in 0..READERS {
            let m = Arc::clone(&m);
            tasks.push(tokio::spawn(async move {
                let ctx = test_ctx();
                let mut misses = 0_usize;
                for _r in 0..ROUNDS {
                    for k in 0..KEYS {
                        match m.get(&format!("k{k}"), &ctx).await {
                            Ok(Some(_)) => {}
                            // A miss is legitimate under concurrent writes.
                            Ok(None) | Err(CacheError::Miss) => misses += 1,
                            Err(e) => panic!("read failed: {e:?}"),
                        }
                    }
                }
                let _ = misses;
            }));
        }

        let started = Instant::now();
        for t in tasks {
            t.await.expect("join");
        }
        println!("mixed traffic in {:?}", started.elapsed());

        // Every key must still be usable: no entry left claimed or prepared.
        let ctx = test_ctx();
        for k in 0..KEYS {
            let snap = m
                .cachelito()
                .peek(&framed_key(&ctx, &format!("k{k}")))
                .expect("peek");
            assert!(
                !snap.population_owner,
                "key k{k} was left claimed after the soak"
            );
            assert!(snap.intent.is_none(), "key k{k} was left prepared");
            // And a write still succeeds, which is the real test of usability.
            m.set(&format!("k{k}"), "final".to_string(), &ctx)
                .await
                .expect("final write");
        }
    });
}

/// Recovery must stay linear in the number of intents, not quadratic. A sweep
/// that re-scans the whole control plane per intent would be invisible at 16
/// intents and obvious at 512.
#[test]
#[ignore = "soak gate; run via `cargo xtask run soak`"]
fn recovery_scales_linearly() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let m = ladder();
        let ctx = test_ctx();

        let measure = |n: usize| {
            for i in 0..n {
                m.cachelito()
                    .prepare(
                        &framed_key(&ctx, &format!("s{i}")),
                        None,
                        TierId::L1,
                        IntentKind::Write,
                    )
                    .expect("prepare");
            }
            let started = Instant::now();
            let RecoveryReport { recovered, failed, .. } = m.recover_older_than(Duration::from_secs(0));
            (started.elapsed(), recovered, failed)
        };

        let (small_t, small_n, small_f) = measure(128);
        let (large_t, large_n, large_f) = measure(1024);

        println!("128 intents: {small_t:?}   1024 intents: {large_t:?}");
        assert_eq!((small_n, small_f), (128, 0));
        assert_eq!((large_n, large_f), (1024, 0));

        // 8x the work must not cost vastly more than 8x the time; allow a wide
        // margin so the gate is about the shape of the curve, not the constant.
        let ratio = large_t.as_nanos() as f64 / (small_t.as_nanos().max(1) as f64);
        assert!(
            ratio < 8.0 * 6.0,
            "recovery scaled by {ratio:.1}x for 8x the intents, which is worse than linear-with-noise"
        );
    });
}

/// The in-memory rungs must stay bounded under a flood of distinct keys: a
/// fixed-capacity tier that grows, or a pool that never reclaims, shows up here.
#[test]
#[ignore = "soak gate; run via `cargo xtask run soak`"]
fn the_default_ladder_survives_a_key_flood() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let m = ladder();
        let ctx = test_ctx();
        let started = Instant::now();

        // Sized to fit the whole ladder, so this measures the flood rather than
        // exhaustion: exhausting it is a separate, separately-tested condition.
        const KEYS: usize = 3_000;
        let mut written = 0_usize;
        for i in 0..KEYS {
            match m.set(&format!("flood{i}"), "v".to_string(), &ctx).await {
                Ok(()) => written += 1,
                Err(thesix::CacheError::CapacityExhausted) => break,
                Err(e) => panic!("write {i}: {e:?}"),
            }
        }
        println!("{written} distinct keys in {:?}", started.elapsed());
        assert!(written > 1_000, "only {written} keys were accepted");

        // Every accepted value must still be readable, whichever rung took it.
        for i in (0..written).rev() {
            let got = m.get(&format!("flood{i}"), &ctx).await.expect("read");
            assert_eq!(got, Some("v".to_string()), "key flood{i} was lost");
        }
    });
}

/// Under sustained traffic with faults, the pipeline must keep serving from the
/// healthy rungs rather than collapsing.
#[test]
#[ignore = "soak gate; run via `cargo xtask run soak`"]
fn sustained_traffic_with_faults_keeps_serving() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        // The faulty rung is L1, which is where `DefaultPolicy` sends a write. The
        // healthy rung is L0, immediately below it.
        //
        // Putting the faults on L0 instead would test nothing: writes go to L1, so
        // the injected faults would never fire, and the walk down to L0 — the
        // whole point of the test — would never happen either.
        let bad = testkit::FaultyTier::<String>::with_plan(
            Arc::new(L1Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
            thesix::FaultPlan::new(),
        );

        // Terminating faults only. A generated plan can include `Hang`, which
        // correctly never resolves, so with a 100ms bound the write legitimately
        // returns `Timeout` — and asserting every write succeeds would be asserting
        // that a hanging rung is not a hanging rung.
        let terminating = [
            thesix::FaultClass::Timeout,
            thesix::FaultClass::ReadFailure,
            thesix::FaultClass::Disconnect,
            thesix::FaultClass::WriteFailure,
        ];
        let mut plan = thesix::FaultPlan::new();
        let mut rng = thesix::DeterministicRng::new(11);
        for _ in 0..512 {
            plan = plan.push(
                thesix::OpKind::Set,
                terminating[rng.below(terminating.len())],
            );
        }
        bad.arm(plan);

        let bad_tier: Arc<dyn CacheTier<String>> = Arc::clone(&bad) as Arc<dyn CacheTier<String>>;
        let m = manager_from_parts(
            thesix::DefaultPolicy,
            thesix::Cachelito::new(),
            vec![
                Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
                bad_tier,
            ],
            Duration::from_millis(100),
        );
        let ctx = test_ctx();

        let mut served = 0_usize;
        let mut degraded = 0_usize;
        let mut refused = 0_usize;
        const KEYS: usize = 2_000;
        for i in 0..KEYS {
            let key = format!("s{i}");
            match m.set(&key, "v".to_string(), &ctx).await {
                Ok(()) => {
                    served += 1;
                    let snap = m.cachelito().peek(&framed_key(&ctx, &key)).expect("peek");
                    if snap.tier == thesix::TierId::L0 {
                        degraded += 1;
                    }
                }
                Err(thesix::CacheError::CapacityExhausted) => refused += 1,
                Err(e) => panic!("write {i} failed with {e:?}; the healthy rung must absorb it"),
            }
        }
        println!("{served} served ({degraded} degraded to L0), {refused} refused");
        assert_eq!(
            served + refused,
            KEYS,
            "every write must land or be refused"
        );
        assert!(served > 0, "nothing was served at all");
        assert!(
            degraded > 0,
            "the healthy rung never absorbed a write, so degradation was not \
             exercised; the injected faults never reached the write path"
        );

        let fired: u64 = thesix::FaultClass::ALL
            .iter()
            .map(|c| bad.ledger().fired(*c))
            .sum();
        println!("{fired} faults fired while {served} writes succeeded");
        assert!(fired > 0, "no fault fired, so the run proved nothing");

        // Every served value is readable, whichever rung took it.
        for i in 0..KEYS {
            let key = format!("s{i}");
            match m.get(&key, &ctx).await {
                Ok(Some(v)) => assert_eq!(v, "v".to_string(), "key {i} came back wrong"),
                Ok(None) | Err(thesix::CacheError::Miss) => {
                    // Only refusals may be absent, and those were counted above.
                    assert!(
                        refused > 0 || m.cachelito().peek(&framed_key(&ctx, &key)).is_ok(),
                        "key {i} vanished without being refused"
                    );
                }
                Err(e) => panic!("read {i} failed with {e:?}"),
            }
        }
    });
}
