#![allow(unused_imports)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use thesix::{CacheError, CacheTier, Cachelito, DefaultPolicy, KeyRef, MemoryPool, TierRegistry};

use common::{
    RecordingTier, make_manager, make_manager_with_timeout, manager_from_tiers, test_ctx,
};
use testkit::ThiefTier;
use thesix::CacheManager;

#[tokio::test]
async fn test_concurrent_readers() {
    let manager = make_manager(DefaultPolicy);
    let key = "shared-key".to_string();

    manager
        .set(&key, "shared-value".to_string(), &test_ctx())
        .await
        .unwrap();

    let mut handles = vec![];
    for _ in 0..50 {
        let m = Arc::clone(&manager);
        let k = key.clone();
        handles.push(tokio::spawn(async move {
            m.get(&k, &test_ctx()).await.unwrap()
        }));
    }

    for handle in handles {
        let result = handle.await.unwrap();
        assert_eq!(result, Some("shared-value".to_string()));
    }
}

#[tokio::test]
async fn test_concurrent_writer_readers() {
    let manager = make_manager(DefaultPolicy);
    let key = "rw-key".to_string();

    let mut handles = vec![];

    for i in 0..10 {
        let m = Arc::clone(&manager);
        let k = key.clone();
        handles.push(tokio::spawn(async move {
            if i % 3 == 0 {
                m.set(&k, format!("{i}"), &test_ctx()).await.unwrap();
            } else {
                let _ = m.get(&k, &test_ctx()).await;
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_concurrent_unrelated_keys() {
    let manager = make_manager(DefaultPolicy);

    let mut handles = vec![];
    for i in 0..20 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            let key = format!("{i}");
            m.set(&key, i.to_string(), &test_ctx()).await.unwrap();
            let result = m.get(&key, &test_ctx()).await.unwrap();
            assert_eq!(result, Some(i.to_string()));
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_shard_contention() {
    let manager = make_manager_with_timeout(DefaultPolicy, Duration::from_secs(2));

    let mut handles = vec![];
    for i in 0..50 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            let shard_idx = i % 4;
            let key = format!("shard-key-{shard_idx}");
            m.set(&key, i.to_string(), &test_ctx()).await.unwrap();
            let result = m.get(&key, &test_ctx()).await.unwrap();
            assert!(result.is_some());
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// B22 / D1: a lost commit race must retry its own rung, never descend.
//
// `set` resolves to L1, so a descent is observable at L0. The test forces a
// genuine lost race -- two writers, both holding live tokens, both inside the
// tier's `set` at the same moment via a rendezvous -- and then asserts that
// L0 was never written. Before the fix, exhausting the retry budget walked down
// to L0 and put a *losing, older* value in a colder tier.
// ---------------------------------------------------------------------------

/// A tier that holds every armed caller at a barrier until the expected number
/// have arrived, guaranteeing both writers are inside `set` simultaneously.
///
/// This is what makes the race deterministic rather than probable: without it,
/// "two tasks writing one key" almost never overlaps enough to lose a commit,
/// which is exactly why this bug survived to a merged `main`.
struct RendezvousTier {
    inner: Arc<dyn CacheTier<String>>,
    gate: Arc<tokio::sync::Barrier>,
    /// Cleared once the rendezvous has happened, so the loser's retry walks
    /// straight through instead of deadlocking on a barrier nobody else joins.
    armed: std::sync::atomic::AtomicBool,
}

impl RendezvousTier {
    fn wrap(inner: Arc<dyn CacheTier<String>>, parties: usize) -> Arc<Self> {
        Arc::new(Self {
            inner,
            gate: Arc::new(tokio::sync::Barrier::new(parties)),
            armed: std::sync::atomic::AtomicBool::new(true),
        })
    }
}

#[async_trait::async_trait]
impl CacheTier<String> for RendezvousTier {
    fn name(&self) -> String {
        self.inner.name()
    }
    fn backend(&self) -> thesix::BackendKind {
        self.inner.backend()
    }
    fn health(&self) -> thesix::TierHealth {
        self.inner.health()
    }
    fn tier_id(&self) -> thesix::TierId {
        self.inner.tier_id()
    }

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<String>, CacheError> {
        self.inner.get(key).await
    }

    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: String,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
            self.gate.wait().await;
            self.armed.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        self.inner.set(key, value, ttl).await
    }

    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.inner.remove(key).await
    }

    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.inner.contains(key).await
    }
}

/// B22 / D1 + D2: a writer that keeps losing must retry its own rung, and when
/// the budget finally runs out it must report `WriteContended` -- never
/// `StaleGeneration`, and never by silently writing to a colder rung.
///
/// The thief makes the loss deterministic. That matters: a single lost race is
/// absorbed by even the old two-attempt budget, so a test that only forces one
/// race passes against the broken code. Only *exhausting* the budget exposes
/// the descent, which is what this test forces.
#[tokio::test]
async fn a_writer_that_keeps_losing_retries_its_rung_and_reports_write_contended() {
    use std::sync::atomic::Ordering::SeqCst;

    let key = "contended".to_string();
    let thief = ThiefTier::wrap(Arc::new(thesix::L0Stub::<String>::new()), &key);
    let l0_rec = RecordingTier::wrap(Arc::new(thesix::L0Stub::<String>::new()));
    let l0_tally = l0_rec.tally();

    let m = manager_from_tiers(
        DefaultPolicy,
        vec![l0_rec, RecordingTier::wrap(thief.clone())],
        Duration::from_millis(250),
    );
    thief.attach(&m);

    let result = m.set(&key, "mine".to_string(), &test_ctx()).await;

    // Anti-vacuity: the thief must actually have stolen, repeatedly. Without
    // this the assertions below would pass against a tier that never raced.
    let steals = thief.steals();
    assert!(
        steals >= 2,
        "the thief must win more than one race for this test to mean anything, got {steals}"
    );

    // D1 first, because it is the more fundamental property and the more
    // dangerous failure: descending does not merely report the wrong error, it
    // *succeeds* by putting a losing, older value in a colder rung.
    assert_eq!(
        l0_tally.writes.load(SeqCst),
        0,
        "a lost race must never descend: L0 was written {l0_tally:?} (result {result:?})",
    );

    // D2: the exhausted budget surfaces as `WriteContended`, never as a
    // control-plane error the public API promises not to leak.
    assert_eq!(
        result,
        Err(CacheError::WriteContended),
        "an exhausted race budget must report WriteContended"
    );
}

/// B22: a *single* lost race is absorbed silently -- both writers succeed and
/// the value lands on the rung the write started on. This is the regression
/// guard for the ordinary case the soak layer used to break.
#[tokio::test]
async fn a_single_lost_race_is_absorbed_and_both_writers_succeed() {
    use std::sync::atomic::Ordering::SeqCst;

    let l0_rec = RecordingTier::wrap(Arc::new(thesix::L0Stub::<String>::new()));
    let l0_tally = l0_rec.tally();
    let l1_inner = Arc::new(thesix::L0Stub::<String>::new());
    // The tally must come from the wrapper the manager actually holds, not from
    // a throwaway one -- otherwise it reads zero forever and asserts nothing.
    let l1_rec = RecordingTier::wrap(RendezvousTier::wrap(l1_inner.clone(), 2));
    let l1_tally = l1_rec.tally();

    let m = manager_from_tiers(
        DefaultPolicy,
        vec![l0_rec, l1_rec],
        Duration::from_millis(250),
    );

    let key = "raced".to_string();
    let (a, b) = tokio::join!(
        async { m.set(&key, "from-a".to_string(), &test_ctx()).await },
        async { m.set(&key, "from-b".to_string(), &test_ctx()).await }
    );

    assert!(
        a.is_ok() && b.is_ok(),
        "both writers must succeed, got {a:?} {b:?}"
    );
    assert!(
        l1_tally.writes.load(SeqCst) > 0,
        "the originating rung must have been written, got l1={} l0={}",
        l1_tally.writes.load(SeqCst),
        l0_tally.writes.load(SeqCst)
    );
    assert_eq!(
        l0_tally.writes.load(SeqCst),
        0,
        "a lost race must retry its own rung, not descend"
    );

    let framed = testkit::framed_key(&test_ctx(), &key);
    let stored = l1_inner
        .get(&KeyRef(framed.as_slice()))
        .await
        .expect("l1 read")
        .expect("a value must be readable at the originating rung");
    assert!(
        stored == "from-a" || stored == "from-b",
        "unexpected stored value {stored:?}"
    );
}

// ---------------------------------------------------------------------------
// B19: can a caller be handed a value from a rung the control plane has
// stopped describing?
//
// `get` peeks, and if it saw `Ready` it awaits the tier. B19 records that
// nothing revalidates the peek's generation across that await, so an `abort`
// landing in the window is invisible to the caller. `GateTier` holds the read
// open so the abort can be placed *deterministically* inside the window rather
// than hoped for.
// ---------------------------------------------------------------------------

/// A tier that parks in `get` until released, so a control-plane mutation can
/// be interleaved into the exact window between peek and the tier read.
struct GateTier {
    inner: Arc<dyn CacheTier<String>>,
    reached: Arc<std::sync::atomic::AtomicBool>,
    gate: Arc<tokio::sync::Notify>,
}

impl GateTier {
    fn wrap(inner: Arc<dyn CacheTier<String>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            reached: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            gate: Arc::new(tokio::sync::Notify::new()),
        })
    }
    fn reached(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.reached)
    }
    fn gate(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.gate)
    }
}

#[async_trait::async_trait]
impl CacheTier<String> for GateTier {
    fn name(&self) -> String {
        self.inner.name()
    }
    fn backend(&self) -> thesix::BackendKind {
        self.inner.backend()
    }
    fn health(&self) -> thesix::TierHealth {
        self.inner.health()
    }
    fn tier_id(&self) -> thesix::TierId {
        self.inner.tier_id()
    }

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<String>, CacheError> {
        self.reached
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.gate.notified().await;
        self.inner.get(key).await
    }

    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: String,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        self.inner.set(key, value, ttl).await
    }
    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.inner.remove(key).await
    }
    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.inner.contains(key).await
    }
}

/// The settling test for B19. It asserts what the caller actually observes; the
/// *interpretation* of that observation is what decides the finding's fate, and
/// the assertion is deliberately about observable behaviour rather than about
/// internals so it cannot pass by accident.
#[tokio::test]
async fn a_read_that_overlaps_an_abort_reports_what_the_control_plane_now_says() {
    use std::sync::atomic::Ordering::SeqCst;

    let key = "overlap".to_string();
    let ctx = test_ctx();
    let framed = testkit::framed_key(&ctx, &key);

    let gate = GateTier::wrap(Arc::new(thesix::L0Stub::<String>::new()));
    let reached = gate.reached();
    let release = gate.gate();

    let m = manager_from_tiers(
        DefaultPolicy,
        vec![RecordingTier::wrap(gate)],
        Duration::from_millis(500),
    );

    // Seed a value the parked read will be able to see.
    // `set` resolves to L1, so seed through the same manager then read at L0.
    m.set(&key, "seeded".to_string(), &ctx).await.expect("seed");

    let reader = {
        let m = Arc::clone(&m);
        let k = key.clone();
        let c = ctx.clone();
        tokio::spawn(async move { m.get(&k, &c).await })
    };

    // Wait until the reader is parked *inside* the tier read, which is strictly
    // after its peek observed `Ready`.
    while !reached.load(SeqCst) {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Anti-vacuity: the control plane must agree the entry was readable, so the
    // abort below is genuinely changing the entry's described state.
    let before = m.cachelito().peek(&framed).expect("peek before abort");
    assert!(
        matches!(before.state, thesix::EntryState::Ready),
        "precondition: the entry must be Ready before the abort, saw {:?}",
        before.state
    );

    // Land an abort in the window. `abort` leaves the entry `Failed` and
    // advances the generation (B6).
    let token = m
        .cachelito()
        .prepare(&framed, None, thesix::TierId::L0, thesix::IntentKind::Write)
        .expect("prepare an intent to abort");
    m.cachelito()
        .abort(&token, CacheError::PopulationFailed)
        .expect("abort");

    let during = m.cachelito().peek(&framed).expect("peek after abort");
    assert!(
        matches!(during.state, thesix::EntryState::Failed),
        "precondition: the abort must have left the entry Failed, saw {:?}",
        during.state
    );
    assert_ne!(
        during.generation, before.generation,
        "precondition: abort must advance the generation"
    );

    // Release the parked read and observe what the caller is handed.
    release.notify_waiters();
    let observed = reader.await.expect("reader task");

    // THE SETTLEMENT. The control plane now describes this entry as `Failed`
    // at a newer generation. If the caller still receives the value, then a
    // value is observable from a rung the control plane has stopped describing
    // -- exactly what B19 says is unproven.
    if let Ok(Some(v)) = observed {
        panic!(
            "B19 CONFIRMED as a defect: the caller received {v:?} while the \
             control plane describes the entry as {:?} at generation {} \
             (was {:?} at {})",
            during.state, during.generation, before.state, before.generation
        );
    }

    // Post-condition: the key must never again serve a value the control plane
    // disowns. A `Failed` entry reports `Miss` (an `Err`), matching the
    // `InFlight` arm, so both miss shapes are acceptable and only a value is not.
    let after = m.get(&key, &ctx).await;
    assert!(
        !matches!(after, Ok(Some(_))),
        "a Failed entry must not serve a value afterwards, got {after:?}"
    );
}
