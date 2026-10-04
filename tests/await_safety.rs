//! The no-control-guard-across-`.await` invariant, proven rather than asserted.
//!
//! `Cachelito`'s `acquire()` returns an owned `ControlSnapshot`, so no shard
//! guard should ever be alive across a tier await. Making the tiers async in 1.0
//! turned that from a convention into a load-bearing property: if a guard did
//! escape, the second task needing the same shard would block forever, and the
//! failure would present as a hang in production rather than a compile error.
//!
//! So this test does not check the types. It stages an await that never
//! completes and asserts that the control plane stays responsive behind it.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use thesix::{
    BackendKind, CacheContext, CacheError, CacheManager, CacheTier, Cachelito, DefaultPolicy,
    IdentityContext, KeyRef, MemoryPool, TierHealth, TierId, TierRegistry,
};

/// A tier whose reads park forever until released. It is the "I/O that never
/// completes" case, which is the only way to observe a leaked guard.
struct HangingTier {
    id: TierId,
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static> CacheTier<V> for HangingTier {
    fn name(&self) -> String {
        format!("{}-hanging", self.id)
    }

    fn backend(&self) -> BackendKind {
        BackendKind::Unavailable
    }

    async fn get(&self, _key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        // Never resolves. A guard held across here would wedge the shard.
        std::future::pending::<()>().await;
        unreachable!("pending() never resolves")
    }

    async fn set(
        &self,
        _key: &KeyRef<'_>,
        _value: V,
        _ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        Ok(())
    }

    async fn remove(&self, _key: &KeyRef<'_>) -> Result<(), CacheError> {
        Ok(())
    }

    async fn contains(&self, _key: &KeyRef<'_>) -> Result<bool, CacheError> {
        Ok(false)
    }

    fn health(&self) -> TierHealth {
        TierHealth::default()
    }

    fn tier_id(&self) -> TierId {
        self.id
    }
}

/// Every rung parks on `get`, because policy decides which tier a `set` lands on
/// and a single hanging tier only proves the invariant for keys routed there.
/// `set` stays instant so the test can still seed a Ready entry.
fn manager_with_hanging_tiers() -> Arc<CacheManager<String, String, DefaultPolicy>> {
    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
        Arc::new(HangingTier { id: TierId::L0 }),
        Arc::new(HangingTier { id: TierId::L1 }),
        Arc::new(HangingTier { id: TierId::L2 }),
        Arc::new(HangingTier { id: TierId::L3 }),
        Arc::new(HangingTier { id: TierId::L4 }),
        Arc::new(HangingTier { id: TierId::L5 }),
    ];
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    Arc::new(CacheManager::new(
        DefaultPolicy,
        cachelito,
        tier_registry,
        tiers,
        pool,
    ))
}

fn test_ctx() -> CacheContext {
    CacheContext::new(IdentityContext::new(
        "test-principal".to_string(),
        vec!["tester".to_string()],
        "test-tenant".to_string(),
    ))
}

/// A tier that parks inside its await must not wedge the control plane.
///
/// Task A blocks forever inside `HangingTier::get`. Task B then performs
/// control-plane work. If any shard guard were held across A's await, B would
/// block on the same mutex and this test would time out rather than fail.
#[tokio::test]
async fn control_plane_stays_responsive_while_a_tier_awaits() {
    let manager = manager_with_hanging_tiers();

    // Seed the entry so the control plane marks it Ready on L0. A fresh key
    // resolves to Miss from the control plane alone and never reaches a tier,
    // so without this the read would return immediately and the test would
    // prove nothing.
    manager
        .set(&"parked".to_string(), "seeded".to_string(), &test_ctx())
        .await
        .expect("seeding the entry must succeed");

    // Park a read inside the tier's await.
    let parked = {
        let m = manager.clone();
        let ctx = test_ctx();
        tokio::spawn(async move { m.get(&"parked".to_string(), &ctx).await })
    };

    // Give it time to reach the pending await.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !parked.is_finished(),
        "the parked read should still be suspended"
    );

    // The control plane must remain usable behind the parked await.
    // `set_tier` is synchronous, so it is run on a blocking thread under a
    // watchdog. A leaked shard guard would leave that thread parked on the
    // mutex and the watchdog would fire.
    let control = {
        let m = manager.clone();
        tokio::task::spawn_blocking(move || m.cachelito().set_tier(b"unrelated-key", TierId::L2))
    };
    let joined = tokio::time::timeout(Duration::from_secs(5), control).await;
    assert!(
        joined.is_ok(),
        "a control-plane write blocked behind a parked tier await: a shard guard is alive across .await"
    );
    assert!(
        joined
            .expect("watchdog fired")
            .expect("task panicked")
            .is_ok()
    );

    // A second, unrelated cache operation on a different key must also proceed.
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        manager.exists(&"different-key".to_string(), &test_ctx()),
    )
    .await;
    assert!(
        second.is_ok(),
        "an unrelated cache operation blocked behind a parked tier await"
    );
}

/// And the reverse: a slow tier must not serialise independent keys, which is
/// the coarse-lock symptom the in-memory shards exist to avoid.
#[tokio::test]
async fn a_parked_tier_does_not_block_other_keys() {
    let manager = manager_with_hanging_tiers();

    manager
        .set(&"key-a".to_string(), "seeded".to_string(), &test_ctx())
        .await
        .expect("seeding the entry must succeed");
    let parked = {
        let m = manager.clone();
        let ctx = test_ctx();
        tokio::spawn(async move { m.get(&"key-a".to_string(), &ctx).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A key whose read lands on L1 (a real stub) rather than the hanging L0
    // must still be able to make progress on the control plane.
    let other = {
        let m = manager.clone();
        tokio::task::spawn_blocking(move || m.cachelito().set_tier(b"key-b", TierId::L1))
    };
    let joined = tokio::time::timeout(Duration::from_secs(5), other).await;
    assert!(
        joined.is_ok(),
        "control plane blocked by an unrelated parked tier"
    );
    assert!(
        joined
            .expect("watchdog fired")
            .expect("task panicked")
            .is_ok()
    );

    drop(parked);
}

/// Sensitivity check for the watchdog used above.
///
/// A test that asserts "the control plane did not block" is only meaningful if
/// the harness would notice if it did. This deliberately parks a
/// `spawn_blocking` thread on a held mutex and asserts the same timeout fires.
/// If this test ever passes silently, the two above have stopped proving
/// anything and should be treated as decoration.
#[tokio::test]
async fn the_watchdog_detects_a_blocked_control_plane() {
    let lock = Arc::new(std::sync::Mutex::new(()));
    let held = lock.clone();

    // Five seconds, not thirty. Dropping a `spawn_blocking` handle does not abort
    // the task, so the thread runs to completion and tokio's blocking pool cannot
    // shut down until it does — a 30s sleep here added 30s to the gate's wall
    // clock for no extra coverage. It must comfortably outlive the 2s timeout
    // below, or the lock would be released before the assertion.
    let parked = tokio::task::spawn_blocking(move || {
        let _guard = held.lock();
        std::thread::sleep(Duration::from_secs(5));
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let blocked = tokio::task::spawn_blocking({
        let l = lock.clone();
        move || {
            let _guard = l.lock();
        }
    });

    let joined = tokio::time::timeout(Duration::from_secs(2), blocked).await;
    assert!(
        joined.is_err(),
        "the watchdog failed to notice a genuinely blocked thread, so it cannot be trusted to notice a blocked control plane"
    );

    drop(parked);
}

/// `concurrency.nested_block_on = false` must be observed, not assumed.
///
/// A test asserting "we never call `block_on` from async context" is worth nothing
/// unless it would notice if we started. Two halves, and the second is the one that
/// usually goes missing:
///
/// * the crate's own source contains no `block_on` at all — no `Handle::block_on`,
///   no `Runtime::block_on`, no `futures::executor::block_on`;
/// * `block_on` *does* panic when nested, so the property is checkable rather than
///   a matter of opinion. Without this half, a future `block_on` that happened to
///   run on a non-runtime thread would pass a purely textual scan while
///   reintroducing exactly the deadlock the invariant names.
///
/// The second half is why the first half is allowed to be a text scan: it is
/// backed by a mechanism that is known to fail loudly.
#[test]
fn nested_block_on_is_absent_from_the_crate_and_would_be_caught() {
    // Half one: no `block_on` anywhere in the library.
    let mut offenders = Vec::new();
    for entry in
        std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).expect("src is readable")
    {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("source file is readable");
        for (n, line) in text.lines().enumerate() {
            // Skip the prose: this file and the trait docs discuss the hazard by
            // name, and a comment mentioning it is not a call to it.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if trimmed.contains("block_on") {
                offenders.push(format!("{}:{}: {trimmed}", path.display(), n + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "`block_on` appears in the library, where nesting it inside an async context \
         deadlocks:\n{}",
        offenders.join("\n")
    );

    // Half two: nesting is observable. `block_on` panics on a runtime thread, so if
    // the crate ever reintroduced one inside an async call, this is the failure it
    // would produce — asserted here so the panic behaviour cannot itself change
    // silently and leave the scan above checking nothing.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    rt.block_on(async {
        let nested =
            std::panic::catch_unwind(|| tokio::runtime::Handle::current().block_on(async { 1u8 }));
        assert!(
            nested.is_err(),
            "nesting block_on inside async context did not panic, so the invariant is \
             not observable and the scan above would prove nothing"
        );
    });
}
