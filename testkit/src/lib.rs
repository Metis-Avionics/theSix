//! Dev-only test harness for `thesix`.
//!
//! Everything here exists to make one thing possible: a test that *proves* the
//! condition it names. Three instruments do that work.
//!
//! * [`FaultyTier`] wraps a real tier and injects contract faults from a
//!   deterministic [`FaultPlan`], recording every activation in a
//!   [`FaultLedger`]. A test asserts the ledger fired, so an error it did not
//!   cause cannot satisfy it.
//! * [`RecordingTier`] counts operations per rung, which is what turns
//!   "promote moved the entry" from an unfalsifiable claim into an assertion.
//! * [`HangingTier`] parks forever on `pending()`. This is the only way to
//!   honestly test that the control plane stays responsive while the data plane
//!   is stalled, because a sleep-based stall would finish before the assertion.

#![allow(dead_code)]
#![allow(clippy::expect_used, clippy::unwrap_used)]

pub mod coverage;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use thesix::{
    BackendKind, CacheContext, CacheError, CacheManager, CacheTier, Cachelito, DefaultPolicy,
    FaultClass, FaultLedger, FaultPlan, IdentityContext, KeyRef, L0Stub, L1Stub, L2Stub, L3Stub,
    L4Stub, L5Stub, MemoryPool, OpKind, TierHealth, TierId, TierRegistry,
};

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// An authenticated context. Tests that need a specific tenant should build
/// their own with [`ctx_for_tenant`] rather than sharing one.
pub fn test_ctx() -> CacheContext {
    CacheContext::new(IdentityContext::new(
        "test-principal".to_string(),
        vec!["tester".to_string()],
        "test-tenant".to_string(),
    ))
}

/// An authenticated context scoped to a named tenant, for isolation tests.
pub fn ctx_for_tenant(tenant: &str) -> CacheContext {
    CacheContext::new(IdentityContext::new(
        "test-principal".to_string(),
        vec!["tester".to_string()],
        tenant.to_string(),
    ))
}

/// The bytes the manager will actually address for `key` under `ctx`.
///
/// The manager frames the tenant into the key, so anything that pokes the
/// control plane directly — seeding state, peeking, recovering — must frame too.
/// Getting this wrong is silent: the operation lands on a *different* key that
/// simply does not exist, and the test concludes the pipeline is broken.
#[must_use]
pub fn framed_key(ctx: &CacheContext, key: &str) -> Vec<u8> {
    try_framed_key(ctx, key).expect("key fits the frame")
}

/// [`framed_key`], but `None` when the frame does not fit.
///
/// Use this when the key is deliberately oversized — a test asserting that an
/// unencodable key is rejected has no frame to look up, and asking for one would
/// make the *test* panic rather than the operation under test.
#[must_use]
pub fn try_framed_key(ctx: &CacheContext, key: &str) -> Option<Vec<u8>> {
    // A fixed budget matching the manager's, so this helper answers the same
    // question the manager does: does this key fit? Growing the buffer to fit
    // would make an oversized key frame successfully and hide the very
    // rejection under test.
    let mut buf = [0u8; thesix::MAX_KEY_SIZE];
    let key_bytes = key.as_bytes();
    if key_bytes.len() > buf.len() {
        return None;
    }
    let raw = key_bytes;
    let n = thesix::frame_tenant_key(&ctx.identity().tenant, raw, &mut buf).ok()?;
    Some(buf[..n].to_vec())
}

/// An anonymous context, for authn/authz negative tests.
pub fn anon_ctx() -> CacheContext {
    CacheContext::anonymous()
}

// ---------------------------------------------------------------------------
// Manager builders
// ---------------------------------------------------------------------------

/// The default six-rung ladder (L0..L5). L6 is left unbound on purpose: the
/// authority rung has no in-memory implementation, and binding it would be the
/// misrepresentation the capability surface exists to prevent.
pub fn default_tiers<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>()
-> Vec<Arc<dyn CacheTier<V>>> {
    vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ]
}

pub fn make_manager<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    policy: DefaultPolicy,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    make_manager_with_timeout(policy, Duration::from_secs(5))
}

pub fn make_manager_with_timeout<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    policy: DefaultPolicy,
    timeout: Duration,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    Arc::new(CacheManager::with_timeout(
        policy,
        Cachelito::new(),
        TierRegistry::new(),
        default_tiers(),
        pool,
        timeout,
    ))
}

/// Build a manager with a telemetry sink attached.
///
/// The builders on `CacheManager` consume `self`, and the manager is an `Arc` in
/// every real use, so wiring telemetry after construction would otherwise mean
/// reconstructing the whole stack in each test that wants to observe it.
pub fn manager_with_telemetry<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    sink: Arc<dyn thesix::TelemetrySink>,
    timeout: Duration,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    let m = CacheManager::with_timeout(
        DefaultPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        default_tiers(),
        pool,
        timeout,
    );
    Arc::new(m.with_telemetry(sink))
}

/// Build a manager over an explicit rung list. Use this when a test needs a
/// specific topology (a fault-injecting rung, an authority rung, a tiny
/// capacity stub) instead of the default ladder.
pub fn manager_from_tiers<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    policy: DefaultPolicy,
    tiers: Vec<Arc<dyn CacheTier<V>>>,
    timeout: Duration,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    Arc::new(CacheManager::with_timeout(
        policy,
        Cachelito::new(),
        TierRegistry::new(),
        tiers,
        pool,
        timeout,
    ))
}

/// A manager over an explicit rung list *and* an explicit control plane.
///
/// Needed when a test must put the control plane into a specific state — a
/// committed entry on a chosen rung, say — before the manager exists. Building
/// that state through the manager instead would route the seeding write through
/// the policy, which sends `set` to L1 and `get` to L0, so the two would disagree
/// about which rung holds the value and a test aiming at one of them would
/// silently exercise the other.
pub fn manager_from_parts<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    policy: DefaultPolicy,
    cachelito: Cachelito,
    tiers: Vec<Arc<dyn CacheTier<V>>>,
    timeout: Duration,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    Arc::new(CacheManager::with_timeout(
        policy,
        cachelito,
        TierRegistry::new(),
        tiers,
        pool,
        timeout,
    ))
}

/// The default ladder with `inner` swapped in at `slot`.
///
/// A test that wants a fault at L3 should say so at the call site rather than
/// counting vector positions inline, which is how the previous suite ended up
/// with `test_tier_failure` hand-wiring index 3.
pub fn ladder_with<V, F>(slot: usize, build: F) -> Vec<Arc<dyn CacheTier<V>>>
where
    V: Clone + Send + Sync + 'static + thesix::IntegrityCheck,
    F: FnOnce() -> Arc<dyn CacheTier<V>>,
{
    let mut tiers = default_tiers::<V>();
    tiers[slot] = build();
    tiers
}

// ---------------------------------------------------------------------------
// RecordingTier
// ---------------------------------------------------------------------------

/// How to damage a stored value, as a named alias so a struct field does not
/// spell out a trait object inline.
pub type Corruptor<V> = Arc<dyn Fn(&mut V) + Send + Sync>;

/// A wrapped ladder plus one tally per rung, in the same order.
pub type RecordedLadder<V> = (Vec<Arc<dyn CacheTier<V>>>, Vec<Arc<Tally>>);

/// Per-rung operation counters. Shared across tiers so a test can assert both
/// the absolute counts and the ratio that makes the claim non-vacuous.
#[derive(Debug, Default)]
pub struct Tally {
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    pub removes: AtomicU64,
    pub contains: AtomicU64,
}

impl Tally {
    pub fn total(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
            + self.writes.load(Ordering::SeqCst)
            + self.removes.load(Ordering::SeqCst)
            + self.contains.load(Ordering::SeqCst)
    }
}

/// Wraps a tier and reports a chosen [`OperationalState`], delegating everything
/// else.
///
/// Exists because [`OperationalState`] has six members and the in-tree stubs only
/// ever produce one of them. `capabilities.must_distinguish.recovering_from_available`
/// cannot be proven by a tier that cannot report the state under test, so the gap
/// was not "no test exists" but "no test could exist".
///
/// Only `state` is overridden. `backend` stays whatever the inner tier reports,
/// so a test cannot accidentally claim a backend the rung was not built with.
pub struct StatedTier<V> {
    inner: Arc<dyn CacheTier<V>>,
    state: thesix::OperationalState,
}

impl<V> StatedTier<V> {
    pub fn wrap(inner: Arc<dyn CacheTier<V>>, state: thesix::OperationalState) -> Arc<Self> {
        Arc::new(Self { inner, state })
    }
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck> CacheTier<V> for StatedTier<V> {
    fn name(&self) -> String {
        format!("stated-{}", self.inner.name())
    }
    fn backend(&self) -> BackendKind {
        self.inner.backend()
    }
    fn capability(&self) -> thesix::capability::TierCapability {
        let mut cap = self.inner.capability();
        cap.state = self.state;
        cap
    }
    fn health(&self) -> TierHealth {
        self.inner.health()
    }
    fn tier_id(&self) -> TierId {
        self.inner.tier_id()
    }
    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        self.inner.get(key).await
    }
    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        self.inner.set(key, value, ttl).await
    }
    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.inner.contains(key).await
    }
    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.inner.remove(key).await
    }
}

/// Wraps a tier and counts every operation that reaches it.
///
/// The anti-vacuity pattern this exists for: asserting `authority_writes == 0`
/// is worthless unless `total_writes > 0` is asserted alongside it, which
/// proves the test actually drove writes through the pipeline.
pub struct RecordingTier<V> {
    inner: Arc<dyn CacheTier<V>>,
    tally: Arc<Tally>,
}

impl<V> RecordingTier<V> {
    pub fn wrap(inner: Arc<dyn CacheTier<V>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            tally: Arc::new(Tally::default()),
        })
    }

    #[must_use]
    pub fn tally(&self) -> Arc<Tally> {
        Arc::clone(&self.tally)
    }
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck> CacheTier<V> for RecordingTier<V> {
    fn name(&self) -> String {
        self.inner.name()
    }
    fn backend(&self) -> BackendKind {
        self.inner.backend()
    }
    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        self.tally.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key).await
    }
    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        self.tally.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.set(key, value, ttl).await
    }
    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.tally.removes.fetch_add(1, Ordering::SeqCst);
        self.inner.remove(key).await
    }
    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.tally.contains.fetch_add(1, Ordering::SeqCst);
        self.inner.contains(key).await
    }
    fn health(&self) -> TierHealth {
        self.inner.health()
    }
    fn tier_id(&self) -> TierId {
        self.inner.tier_id()
    }
}

/// Wrap every rung so a test can prove an operation reached the ladder at all.
pub fn record_all<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    tiers: Vec<Arc<dyn CacheTier<V>>>,
) -> RecordedLadder<V> {
    let mut wrapped = Vec::with_capacity(tiers.len());
    let mut tallies = Vec::with_capacity(tiers.len());
    for t in tiers {
        let rec = RecordingTier::wrap(t);
        tallies.push(rec.tally());
        wrapped.push(rec as Arc<dyn CacheTier<V>>);
    }
    (wrapped, tallies)
}

// ---------------------------------------------------------------------------
// HangingTier
// ---------------------------------------------------------------------------

/// Every operation parks forever on `std::future::pending()`.
///
/// This is a genuine never-completing data-plane operation, not a sleep. A
/// sleep-based stall can finish before the assertion runs, which is how a test
/// named `control_plane_survives_blocked_data_plane` ends up proving nothing.
pub struct HangingTier {
    inner: Arc<dyn CacheTier<String>>,
    reached: Arc<AtomicU64>,
}

impl HangingTier {
    pub fn wrap(inner: Arc<dyn CacheTier<String>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            reached: Arc::new(AtomicU64::new(0)),
        })
    }

    /// How many operations actually reached the stall. Assert this is non-zero
    /// before claiming the data plane was blocked.
    #[must_use]
    pub fn reached(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.reached)
    }
}

#[async_trait::async_trait]
impl CacheTier<String> for HangingTier {
    fn name(&self) -> String {
        "hanging".to_string()
    }
    fn backend(&self) -> BackendKind {
        BackendKind::Test
    }
    async fn get(&self, _key: &KeyRef<'_>) -> Result<Option<String>, CacheError> {
        self.reached.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<Result<_, CacheError>>().await
    }
    async fn set(
        &self,
        _key: &KeyRef<'_>,
        _value: String,
        _ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        self.reached.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<Result<_, CacheError>>().await
    }
    async fn remove(&self, _key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.reached.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<Result<_, CacheError>>().await
    }
    async fn contains(&self, _key: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.reached.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<Result<_, CacheError>>().await
    }
    fn health(&self) -> TierHealth {
        TierHealth::default()
    }
    fn tier_id(&self) -> TierId {
        self.inner.tier_id()
    }
}

// ---------------------------------------------------------------------------
// FaultyTier
// ---------------------------------------------------------------------------

/// A tier that injects contract faults from a deterministic plan and records
/// every activation.
///
/// Corrupting a stored value is the one fault this cannot fake honestly: it
/// corrupts the payload bytes on the way *into* the inner tier, so the tier's
/// own integrity check is what has to notice. A wrapper that merely returned a
/// damaged value would be testing the wrapper.
pub struct FaultyTier<V> {
    inner: Arc<dyn CacheTier<V>>,
    plan: std::sync::Mutex<FaultPlan>,
    ledger: Arc<FaultLedger>,
    remaining_ok: AtomicU64,
    /// Whether an armed plan withdraws `ATOMIC_WRITE_OR_ERROR` from the inner
    /// tier's capability.
    ///
    /// Defaults to true, because a fault-injecting tier is simulating a backend
    /// that is no longer well behaved. A test that needs to prove the manager
    /// *skips* compensation for a tier that does promise atomicity sets this
    /// false — without that, the skip path is untested and the compensating
    /// remove could silently start destroying good values.
    withholds_atomicity: std::sync::atomic::AtomicBool,
    /// Whether a `FailureAfterN` window has been opened yet. Separate from
    /// `remaining_ok`, because a window of `n = 1` legitimately reaches zero
    /// after its single allowed success and must not be re-armed.
    window_armed: AtomicBool,
    corrupt_next: AtomicBool,
    /// How to damage a stored value. Injected rather than assumed so the tier
    /// stays generic over `V`: the alternative was hardcoding `String`, which
    /// would make the harness unusable for any other payload type.
    corruptor: Option<Corruptor<V>>,
}

impl<V> FaultyTier<V> {
    /// Wrap `inner` with an empty plan. Nothing fires until a plan is armed.
    pub fn wrap(inner: Arc<dyn CacheTier<V>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            plan: std::sync::Mutex::new(FaultPlan::new()),
            ledger: Arc::new(FaultLedger::new()),
            remaining_ok: AtomicU64::new(0),
            withholds_atomicity: AtomicBool::new(true),
            window_armed: AtomicBool::new(false),
            corrupt_next: AtomicBool::new(false),
            corruptor: None,
        })
    }

    pub fn with_plan(inner: Arc<dyn CacheTier<V>>, plan: FaultPlan) -> Arc<Self> {
        Arc::new(Self {
            inner,
            plan: std::sync::Mutex::new(plan),
            ledger: Arc::new(FaultLedger::new()),
            remaining_ok: AtomicU64::new(0),
            withholds_atomicity: AtomicBool::new(true),
            window_armed: AtomicBool::new(false),
            corrupt_next: AtomicBool::new(false),
            corruptor: None,
        })
    }

    /// Supply how to damage a stored value, and arm the corruption fault.
    ///
    /// The corruption is applied on the way *into* the wrapped tier, so the
    /// tier's own integrity check is what has to notice it. A wrapper that
    /// merely returned a damaged value on read would be testing the wrapper.
    pub fn with_corruptor(
        inner: Arc<dyn CacheTier<V>>,
        plan: FaultPlan,
        corruptor: impl Fn(&mut V) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            plan: std::sync::Mutex::new(plan),
            ledger: Arc::new(FaultLedger::new()),
            remaining_ok: AtomicU64::new(0),
            withholds_atomicity: AtomicBool::new(true),
            window_armed: AtomicBool::new(false),
            corrupt_next: AtomicBool::new(false),
            corruptor: Some(Arc::new(corruptor)),
        })
    }

    /// The activation ledger. Tests must assert against this.
    #[must_use]
    pub fn ledger(&self) -> Arc<FaultLedger> {
        Arc::clone(&self.ledger)
    }

    /// Arm a plan at runtime.
    pub fn arm(&self, plan: FaultPlan) {
        *self.plan.lock().expect("plan mutex") = plan;
        self.remaining_ok.store(0, Ordering::SeqCst);
        self.window_armed.store(false, Ordering::SeqCst);
    }

    /// Corrupt the next stored payload, flipping its last byte.
    ///
    /// Only meaningful for byte-valued payloads; a `V` with no byte
    /// representation cannot be corrupted this way and the caller should assert
    /// that rather than assume it happened.
    pub fn corrupt_next_write(&self) {
        self.corrupt_next.store(true, Ordering::SeqCst);
    }

    /// Consume the next fault armed for `op`, recording it.
    fn next_fault(&self, op: OpKind) -> Option<(FaultClass, u64)> {
        self.ledger.observe_op();
        let armed = {
            let mut plan = self.plan.lock().expect("plan mutex");
            plan.take_for(op)
        }?;
        let fault = armed.class;
        let amount = armed.amount;

        // A non-zero `amount` on a deliverable error class is a schedule, not a
        // latency: `push_after_n(Set, WriteFailure, 2)` means "two writes
        // succeed, then a write failure". The old code keyed off
        // `matches!(fault, FailureAfterN)`, which never matched, so every
        // scheduled failure fired on the first operation.
        if amount > 0 && fault.is_scheduled_failure() {
            // Succeed `amount` times, then start failing. The window has to be
            // *armed* on first sight, not merely decremented: `remaining_ok`
            // starts at 0, so a plain decrement fires immediately and `n = 2`
            // behaved like `n = 0`.
            let mut plan = self.plan.lock().expect("plan mutex");
            if !self.window_armed.swap(true, Ordering::SeqCst) {
                // Opening the window. This call is itself one of the `amount`
                // allowed successes, so the window starts one short — otherwise
                // `n = 2` permitted three operations.
                self.remaining_ok
                    .store(amount.saturating_sub(1), Ordering::SeqCst);
                plan.rearm(armed);
                return None;
            }
            let prev = self
                .remaining_ok
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                    if cur == 0 { None } else { Some(cur - 1) }
                });
            if prev.is_ok() {
                plan.rearm(armed);
                return None;
            }
            drop(plan);
        }

        self.ledger.record(fault);
        Some((fault, amount))
    }
}

/// The default corruption: flip the last byte, so the value changes shape.
///
/// Only correct for UTF-8-representable values, which is why it is offered as a
/// closure the caller injects rather than hardcoded into the generic tier.
pub fn corrupt_string(value: &mut String) {
    let mut bytes = value.as_bytes().to_vec();
    if let Some(last) = bytes.last_mut() {
        *last ^= 0xFF;
    } else {
        bytes.push(0xFF);
    }
    // Lossy round-trip: if the damaged bytes are no longer valid UTF-8 the
    // value changes shape, which is exactly the corruption we want to detect.
    *value = String::from_utf8_lossy(&bytes).into_owned();
}

fn error_for(fault: FaultClass) -> CacheError {
    match fault {
        FaultClass::Timeout | FaultClass::Latency => CacheError::Timeout,
        FaultClass::ReadFailure | FaultClass::Disconnect => CacheError::TierUnavailable,
        FaultClass::WriteFailure => CacheError::TierUnavailable,
        FaultClass::MetadataFailure => CacheError::TierUnavailable,
        FaultClass::CapacityExhaustion => CacheError::CapacityExhausted,
        FaultClass::Corruption => CacheError::Corrupted,
        FaultClass::Cancellation => CacheError::Cancelled,
        FaultClass::Hang | FaultClass::FailureAfterN => CacheError::TierUnavailable,
        FaultClass::PartialWrite => CacheError::WriteIndeterminate,
    }
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck> CacheTier<V> for FaultyTier<V> {
    fn name(&self) -> String {
        format!("faulty-{}", self.inner.name())
    }
    fn backend(&self) -> BackendKind {
        BackendKind::Test
    }
    fn capability(&self) -> thesix::capability::TierCapability {
        // Withhold `ATOMIC_WRITE_OR_ERROR` whenever a plan is armed.
        //
        // Delegating to the inner tier here would be a trap: the stub underneath
        // genuinely does guarantee that a failed write stores nothing, so the
        // manager would skip residue cleanup, and a `PartialWrite` test would then
        // pass while residue accumulated — the fault would be injected and the
        // ledger would record it, yet nothing would be tested.
        //
        // Arming a plan is a statement that this backend is no longer well
        // behaved, so the guarantee goes with it.
        let mut cap = self.inner.capability();
        if self.plan.lock().expect("plan mutex").is_empty() {
            cap.flags = cap
                .flags
                .without(thesix::capability::CapabilityFlags::ATOMIC_WRITE_OR_ERROR);
        }
        cap
    }

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        if let Some((fault, _)) = self.next_fault(OpKind::Get) {
            match fault {
                FaultClass::Hang => std::future::pending::<Result<_, CacheError>>().await,
                FaultClass::Latency => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    self.inner.get(key).await
                }
                _ => Err(error_for(fault)),
            }
        } else {
            self.inner.get(key).await
        }
    }

    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        if let Some((fault, amount)) = self.next_fault(OpKind::Set) {
            match fault {
                FaultClass::Hang => std::future::pending::<Result<_, CacheError>>().await,
                FaultClass::Latency => {
                    tokio::time::sleep(Duration::from_millis(amount.max(1))).await;
                    self.inner.set(key, value, ttl).await
                }
                // Write first, fail second. The inner tier really stores the value
                // and this returns an error anyway, which is the whole point: every
                // other class fails before doing the work.
                FaultClass::PartialWrite => {
                    self.inner.set(key, value, ttl).await?;
                    Err(error_for(fault))
                }
                _ => Err(error_for(fault)),
            }
        } else if self.corrupt_next.swap(false, Ordering::SeqCst) {
            // `clone` rather than a move, because the uncorrupted path still
            // needs the original when no corruptor is wired.
            let Some(corruptor) = self.corruptor.clone() else {
                // No corruptor wired: pass the value through untouched. A test
                // that asked for corruption without supplying a corruptor must
                // fail its own ledger assertion rather than quietly store clean
                // data and pass.
                return self.inner.set(key, value, ttl).await;
            };
            let mut damaged = value.clone();
            corruptor(&mut damaged);
            self.ledger.record(FaultClass::Corruption);
            self.inner.set(key, damaged, ttl).await
        } else {
            self.inner.set(key, value, ttl).await
        }
    }

    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        if let Some((fault, _)) = self.next_fault(OpKind::Remove) {
            match fault {
                FaultClass::Hang => std::future::pending::<Result<_, CacheError>>().await,
                _ => Err(error_for(fault)),
            }
        } else {
            self.inner.remove(key).await
        }
    }

    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        if let Some((fault, _)) = self.next_fault(OpKind::Contains) {
            match fault {
                FaultClass::Hang => std::future::pending::<Result<_, CacheError>>().await,
                _ => Err(error_for(fault)),
            }
        } else {
            self.inner.contains(key).await
        }
    }

    fn health(&self) -> TierHealth {
        if self.ledger.ops_observed() > 0 {
            TierHealth {
                consecutive_failures: 0,
                last_failure_timestamp: None,
                health_score: 1.0,
                availability: 1.0,
            }
        } else {
            self.inner.health()
        }
    }

    fn tier_id(&self) -> TierId {
        self.inner.tier_id()
    }
}

/// Wrap `inner` in a fault tier and return both, so the caller keeps the handle
/// it needs for ledger assertions.
pub fn faulty<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    inner: Arc<dyn CacheTier<V>>,
    plan: FaultPlan,
) -> (Arc<FaultyTier<V>>, Arc<FaultLedger>) {
    let tier = FaultyTier::with_plan(inner, plan);
    let ledger = tier.ledger();
    (tier, ledger)
}

/// As [`faulty`], with a corruption function wired in.
pub fn faulty_corrupting<V: Clone + Send + Sync + 'static + thesix::IntegrityCheck>(
    inner: Arc<dyn CacheTier<V>>,
    plan: FaultPlan,
    corruptor: impl Fn(&mut V) + Send + Sync + 'static,
) -> (Arc<FaultyTier<V>>, Arc<FaultLedger>) {
    let tier = FaultyTier::with_corruptor(inner, plan, corruptor);
    let ledger = tier.ledger();
    (tier, ledger)
}

/// A tier that stores the value and *then* reports a failure that does not admit
/// the write might have landed.
///
/// This models the backend behaviour that made `partial_commit_visible = false`
/// reachable, and it is deliberately **not** a [`FaultClass`]. Every
/// `FaultClass` is a contract-named misbehaviour with a declared error, and the
/// contract's answer to "my bytes may have landed" is already
/// `CacheError::WriteIndeterminate` — which [`FaultyTier`] reports honestly via
/// `PartialWrite`. This tier is the case the contract has no name for: a backend
/// that returns an error indistinguishable from a clean rejection *after*
/// storing the value.
///
/// It exists because a manager that trusts such an error will serve the residue,
/// and only a test that leaves real bytes behind can prove it does not. A tier
/// that fails cleanly stores nothing, so asserting against one proves nothing
/// about residue at all — which is how the original gap survived: the write path
/// was tested only with tiers that kept their promises.
/// The control-plane address of `key` under the default placement, which is
/// what every in-tree stub uses unless a test injects another `Placement`.
///
/// Exposed so a test can address a slot the way the manager does, instead of
/// re-deriving the hash and getting it subtly wrong.
#[must_use]
pub fn address_via_manager_fingerprint(key: &[u8]) -> thesix::KeyAddress {
    thesix::KeyAddress::of(key, thesix::Placement::Default)
}

/// A tier that wins every commit race it can reach.
///
/// On every non-re-entrant `set` it performs a competing `manager.set` for the
/// *same application key* first. That commit bumps the generation, so the
/// caller's own token is expired by the time it reaches `commit` and it loses.
/// The re-entrancy guard stops the thief's own write from being stolen from,
/// so exactly one competitor runs per attempt and the loss is deterministic
/// rather than probable.
pub struct ThiefTier {
    inner: Arc<dyn CacheTier<String>>,
    /// The *application* key, not the framed bytes: re-framing `KeyRef` bytes
    /// would address a different key and the theft would silently miss.
    key: String,
    mgr: std::sync::OnceLock<std::sync::Weak<CacheManager<String, String, DefaultPolicy>>>,
    stealing: std::sync::atomic::AtomicBool,
    steals: std::sync::atomic::AtomicU64,
}

impl ThiefTier {
    #[must_use]
    pub fn wrap(inner: Arc<dyn CacheTier<String>>, key: &str) -> Arc<Self> {
        Arc::new(Self {
            inner,
            key: key.to_string(),
            mgr: std::sync::OnceLock::new(),
            stealing: std::sync::atomic::AtomicBool::new(false),
            steals: std::sync::atomic::AtomicU64::new(0),
        })
    }
    pub fn attach(&self, m: &Arc<CacheManager<String, String, DefaultPolicy>>) {
        let _ = self.mgr.set(Arc::downgrade(m));
    }
    #[must_use]
    pub fn steals(&self) -> u64 {
        self.steals.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl CacheTier<String> for ThiefTier {
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
        use std::sync::atomic::Ordering::SeqCst;
        if !self.stealing.swap(true, SeqCst) {
            self.steals.fetch_add(1, SeqCst);
            if let Some(m) = self.mgr.get().and_then(std::sync::Weak::upgrade) {
                let _ = m.set(&self.key, "thief".to_string(), &test_ctx()).await;
            }
            self.stealing.store(false, SeqCst);
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

pub struct MisreportingTier {
    inner: Arc<dyn CacheTier<String>>,
    misreport_next: AtomicBool,
    stored: Arc<AtomicU64>,
}

impl MisreportingTier {
    #[must_use]
    pub fn wrap(inner: Arc<dyn CacheTier<String>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            misreport_next: AtomicBool::new(false),
            stored: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Misreport the next `set`, and only that one.
    pub fn misreport_next(&self) {
        self.misreport_next.store(true, Ordering::SeqCst);
    }

    /// How many writes actually reached the inner tier while misreporting.
    ///
    /// Assert this is non-zero before claiming residue was left behind.
    #[must_use]
    pub fn stored(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.stored)
    }
}

#[async_trait::async_trait]
impl CacheTier<String> for MisreportingTier {
    fn name(&self) -> String {
        format!("misreporting-{}", self.inner.name())
    }
    fn backend(&self) -> BackendKind {
        BackendKind::Test
    }
    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<String>, CacheError> {
        self.inner.get(key).await
    }
    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: String,
        ttl: Option<Duration>,
    ) -> Result<(), CacheError> {
        if self.misreport_next.swap(false, Ordering::SeqCst) {
            // Store first, then report the cleanest possible failure.
            self.inner.set(key, value, ttl).await?;
            self.stored.fetch_add(1, Ordering::SeqCst);
            return Err(CacheError::TierUnavailable);
        }
        self.inner.set(key, value, ttl).await
    }
    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.inner.remove(key).await
    }
    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.inner.contains(key).await
    }
    fn health(&self) -> TierHealth {
        self.inner.health()
    }
    fn tier_id(&self) -> TierId {
        self.inner.tier_id()
    }
}
