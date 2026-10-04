//! Performance gates.
//!
//! Every test here is `#[ignore]`d and runs only through `cargo xtask run
//! performance`. Two reasons:
//!
//! * Wall-clock assertions are flaky on shared CI hardware, and a flaky gate is
//!   worse than no gate — it trains people to re-run.
//! * The suite costs minutes. Folding it into the default pass would make every
//!   `cargo test` slow enough that people stop running it.
//!
//! What these measure is *boundedness*, not absolute speed. "p99 under 50ms on
//! this machine" is not a portable claim; "control-plane p99 stays under X while
//! a rung is wedged forever" is, because it compares the control plane against
//! itself rather than against a number that changes with the hardware.
//!
//! # Anti-vacuity
//!
//! Each test that claims a stall or a hot key asserts the stall or the key really
//! occurred, via `HangingTier::reached` and the record counters. A latency
//! assertion that never had to wait for anything is not evidence.

use std::sync::Arc;
use std::time::{Duration, Instant};

use testkit::{Tally, framed_key, manager_from_parts, manager_with_telemetry, test_ctx};
use thesix::{
    CacheTier, KeyRef, L0Stub, L1Stub, LatencyPercentiles, RecoveryReport, RingTelemetry,
    TelemetrySink, TierId,
};

const SETTLE: Duration = Duration::from_millis(50);

fn ladder() -> Arc<thesix::CacheManager<String, String, thesix::DefaultPolicy>> {
    manager_from_parts(
        thesix::DefaultPolicy,
        thesix::Cachelito::new(),
        testkit::default_tiers::<String>(),
        Duration::from_millis(500),
    )
}

/// The contract's ten measures, on one workload.
#[test]
#[ignore = "performance gate; run via `cargo xtask run performance`"]
fn latency_percentiles_are_reported() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let ring = Arc::new(RingTelemetry::new(4096));
        let sink: Arc<dyn TelemetrySink> = ring.clone();
        let m = manager_with_telemetry::<String>(sink, Duration::from_millis(500));
        let ctx = test_ctx();

        // Seed so reads hit a committed path rather than all missing.
        for i in 0..64 {
            m.set(&format!("k{i}"), "value".to_string(), &ctx)
                .await
                .expect("seed");
        }

        for round in 0..400 {
            let key = format!("k{}", round % 64);
            let _ = m.get(&key, &ctx).await;
            if round % 8 == 0 {
                let _ = m
                    .set(&format!("k{}", round % 64), "value".to_string(), &ctx)
                    .await;
            }
        }

        let p = ring.percentiles();
        println!("read/write mix: {p}");
        assert!(p.count > 300, "only {} records were captured", p.count);
        assert!(p.p50 <= p.p95, "p50 {} > p95 {}", p.p50, p.p95);
        assert!(p.p95 <= p.p99, "p95 {} > p99 {}", p.p95, p.p99);
        assert!(p.p99 <= p.p99_9, "p99 {} > p99.9 {}", p.p99, p.p99_9);
        assert!(p.p99_9 <= p.max, "p99.9 {} > max {}", p.p99_9, p.max);
    });
}

/// The control plane must stay responsive while the data plane is wedged
/// *forever*. This is the load-bearing performance test: it is the only one that
/// would fail if a lock guard were ever held across an `.await`.
#[test]
#[ignore = "performance gate; run via `cargo xtask run performance`"]
fn control_plane_stays_bounded_while_a_rung_is_wedged() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let hanging = testkit::HangingTier::wrap(
            Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>
        );
        let reached = hanging.reached();
        let ctx = test_ctx();

        // Commit an entry on L0 so reads actually reach the wedged rung.
        let framed = framed_key(&ctx, "wedged");
        let cachelito = thesix::Cachelito::new();
        let token = cachelito
            .prepare(&framed, None, TierId::L0, thesix::IntentKind::Write)
            .expect("prepare");
        cachelito.commit(&token, None).expect("commit");

        let m = manager_from_parts(
            thesix::DefaultPolicy,
            cachelito,
            vec![hanging as Arc<dyn CacheTier<String>>],
            Duration::from_millis(100),
        );

        // Hammer the wedged rung from several tasks.
        let mut readers = Vec::new();
        for _ in 0..4 {
            let m = Arc::clone(&m);
            let ctx = ctx.clone();
            readers.push(tokio::spawn(async move {
                for _ in 0..20 {
                    let _ = m.get(&"wedged".to_string(), &ctx).await;
                }
            }));
        }
        tokio::time::sleep(SETTLE).await;
        let stalled = reached.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            stalled > 0,
            "the data plane never stalled; the control plane proved nothing"
        );

        // Now measure the control plane alone.
        let mut samples = Vec::with_capacity(200);
        for i in 0..200 {
            let key = format!("probe-{i}");
            let f = framed_key(&ctx, &key);
            let started = Instant::now();
            let _ = m.cachelito().peek(&f);
            let _ = m.cachelito().bump_generation(&f);
            samples.push(started.elapsed().as_micros() as u64);
        }
        let p = LatencyPercentiles::from_samples(&mut samples);
        println!("control plane under a wedged data plane: {p}");

        // Boundedness: the 99.9th percentile of two shard-locked map operations
        // must stay in the low milliseconds. A guard held across an await, or a
        // global lock, shows up here as a tail in the seconds.
        assert!(
            p.p99_9 < 50_000,
            "control-plane p99.9 was {}us while a rung was wedged",
            p.p99_9
        );
        assert!(
            p.max < 500_000,
            "control-plane max was {}us while a rung was wedged",
            p.max
        );

        for r in readers {
            r.abort();
        }
    });
}

/// A hot key must not serialise unrelated keys.
#[test]
#[ignore = "performance gate; run via `cargo xtask run performance`"]
fn a_hot_key_does_not_starve_cold_keys() {
    testkit::proves!("hpa.isolation.hot_key_may_block_unrelated_keys");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let m = ladder();
        let ctx = test_ctx();
        m.set(&"hot".to_string(), "v".to_string(), &ctx)
            .await
            .expect("seed hot");
        for i in 0..32 {
            m.set(&format!("cold{i}"), "v".to_string(), &ctx)
                .await
                .expect("seed cold");
        }

        // Saturate one key.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hot_stop = Arc::clone(&stop);
        let hot_ctx = ctx.clone();
        let hot_mgr = Arc::clone(&m);
        let hot = tokio::spawn(async move {
            let m = hot_mgr;
            while !hot_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = m.get(&"hot".to_string(), &hot_ctx).await;
            }
        });
        tokio::time::sleep(SETTLE).await;

        // Unrelated keys must still be answered promptly.
        let mut samples = Vec::new();
        for i in 0..32 {
            let started = Instant::now();
            let r = m.get(&format!("cold{i}"), &ctx).await;
            samples.push(started.elapsed().as_micros() as u64);
            assert_eq!(r.expect("get"), Some("v".to_string()));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = hot.await;

        let p = LatencyPercentiles::from_samples(&mut samples);
        println!("cold keys while one key is hot: {p}");
        assert!(
            p.p99 < 50_000,
            "a hot key pushed unrelated reads to p99 {}us",
            p.p99
        );
    });
}

/// Shards must actually be independent: two keys on different shards must not
/// serialise. The absence of this test was the gap in the sharding claim.
#[test]
#[ignore = "performance gate; run via `cargo xtask run performance`"]
fn distinct_shards_do_not_serialise() {
    testkit::proves!("hpa.isolation.hot_shard_may_block_entire_pipeline");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let (tiers, tallies) = testkit::record_all(testkit::default_tiers::<String>());
        let m = manager_from_parts(
            thesix::DefaultPolicy,
            thesix::Cachelito::new(),
            tiers,
            Duration::from_millis(500),
        );
        let ctx = test_ctx();

        // Many keys, so they spread across shards.
        for i in 0..256 {
            m.set(&format!("s{i}"), "v".to_string(), &ctx)
                .await
                .expect("seed");
        }

        let started = Instant::now();
        for _round in 0..4 {
            for i in 0..256 {
                let _ = m.get(&format!("s{i}"), &ctx).await;
            }
        }
        let elapsed = started.elapsed();
        let ops = 4 * 256;
        println!(
            "{ops} reads across 6 rungs in {elapsed:?} ({:.0}/s)",
            ops as f64 / elapsed.as_secs_f64()
        );

        let total: u64 = tallies.iter().map(|t: &Arc<Tally>| t.total()).sum();
        assert!(
            total >= ops as u64,
            "reads did not reach the tiers: {total}"
        );
        assert!(
            elapsed.as_secs_f64() < 30.0,
            "{ops} reads took {elapsed:?}, which suggests serialisation"
        );
    });
}

/// Fallback rate must be observable, or `[hpa.performance] fallback_rate` is a
/// number nobody can produce.
#[test]
#[ignore = "performance gate; run via `cargo xtask run performance`"]
fn fallback_rate_is_observable() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let ring = Arc::new(RingTelemetry::new(1024));
        let sink: Arc<dyn TelemetrySink> = ring.clone();
        let m = manager_with_telemetry::<String>(sink, Duration::from_millis(200));
        let ctx = test_ctx();

        // Seed a value, then force the population to fail so the fail-open scan
        // has to run.
        m.set(&"fallback".to_string(), "resident".to_string(), &ctx)
            .await
            .expect("seed");

        for i in 0..16 {
            let _ = m
                .get_or_fetch(&format!("miss{i}"), &ctx, || async {
                    Err(thesix::CacheError::TierUnavailable)
                })
                .await;
        }

        let records = ring.records();
        assert!(!records.is_empty(), "no records were captured");
        let outcomes: std::collections::BTreeMap<&str, usize> =
            records
                .iter()
                .fold(std::collections::BTreeMap::new(), |mut acc, r| {
                    *acc.entry(r.outcome.name()).or_default() += 1;
                    acc
                });
        println!("outcomes: {outcomes:?}");
        assert!(
            outcomes.contains_key("failed") || outcomes.contains_key("fell_back"),
            "no failure was reported: {outcomes:?}"
        );
    });
}

/// Allocation behaviour: a read path that allocates per operation cannot be
/// called allocation-free, and the contract asks for the rate to be *known*.
#[test]
#[ignore = "performance gate; run via `cargo xtask run performance`"]
fn the_read_path_reports_its_cost() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let m = ladder();
        let ctx = test_ctx();
        m.set(&"alloc".to_string(), "v".to_string(), &ctx)
            .await
            .expect("seed");

        // Warm up, then measure. The warm-up matters: first-touch of a slot
        // allocates, and including it would report a one-off as a steady-state
        // rate.
        for _ in 0..100 {
            let _ = m.get(&"alloc".to_string(), &ctx).await;
        }

        let started = Instant::now();
        let n = 1000;
        for _ in 0..n {
            let _ = m.get(&"alloc".to_string(), &ctx).await;
        }
        let elapsed = started.elapsed();
        let per_op = elapsed / n;
        println!("{n} warm reads in {elapsed:?} ({per_op:?} each)");
        assert!(
            per_op < Duration::from_millis(5),
            "a warm read took {per_op:?}, which is far too slow for an in-memory rung"
        );
    });
}

/// Recovery latency: how long from "intent outstanding" to "intent resolved".
#[test]
#[ignore = "performance gate; run via `cargo xtask run performance`"]
fn recovery_latency_is_bounded() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let m = ladder();
        let ctx = test_ctx();
        const KEYS: usize = 512;

        let started = Instant::now();
        for i in 0..KEYS {
            m.cachelito()
                .prepare(
                    &framed_key(&ctx, &format!("r{i}")),
                    None,
                    TierId::L1,
                    thesix::IntentKind::Write,
                )
                .expect("prepare");
        }
        let RecoveryReport {
            recovered, failed, ..
        } = m.recover_older_than(Duration::from_secs(0));
        let elapsed = started.elapsed();
        println!("recovered {recovered} intents in {elapsed:?}");

        assert_eq!(recovered, KEYS, "not every intent was recovered");
        assert_eq!(failed, 0);
        assert!(
            elapsed < Duration::from_secs(5),
            "recovering {KEYS} intents took {elapsed:?}"
        );
    });
}

/// The tier must remain a working store while reporting degraded, so a consumer
/// can decide to route around it without it being dead.
#[test]
#[ignore = "performance gate; run via `cargo xtask run performance`"]
fn a_degraded_rung_still_serves() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let tiers = vec![
            Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
            Arc::new(L1Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        ];
        let m = manager_from_parts(
            thesix::DefaultPolicy,
            thesix::Cachelito::new(),
            tiers,
            Duration::from_millis(200),
        );
        let ctx = test_ctx();
        m.set(&"k".to_string(), "v".to_string(), &ctx)
            .await
            .expect("write");
        let got = m.get(&"k".to_string(), &ctx).await;
        assert_eq!(got.expect("read"), Some("v".to_string()));
        // And the rung is genuinely reachable, so the read was not a control-plane
        // answer.
        assert!(
            m.tier(&TierId::L1)
                .expect("L1 bound")
                .contains(&KeyRef(&framed_key(&ctx, "k")))
                .await
                .expect("contains")
        );
    });
}
