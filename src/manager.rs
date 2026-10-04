use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use crate::capability::TierCapability;
use crate::continuity::RecoveryReport;
use crate::control::cachelito::{Cachelito, ControlSnapshot};
use crate::entry::{CommitToken, EntryState, Generation, IntentKind};
use crate::error::CacheError;
use crate::identity::CacheContext;
use crate::key::Key;
use crate::key::KeyRef;
use crate::policy::{CacheOperation, CachePolicy, CacheRequest, CacheState, FailMode};
use crate::pool::MemoryPool;
use crate::telemetry::{Operation, OperationRecord, SharedSink};
use crate::tier::tier_trait::CacheTier;
use crate::tier::{TierId, TierRegistry};

const MAX_KEY_SIZE: usize = 256;
/// Maximum populate attempts per `stampede.toml` ([retry] `max_attempts`).
const MAX_POPULATE_ATTEMPTS: u32 = 3;
/// Base backoff for populate retries; doubled each attempt (exponential).
const RETRY_BASE_BACKOFF: Duration = Duration::from_millis(50);

/// Releases a population claim unless it was explicitly disarmed.
///
/// Cancellation is invisible to ordinary control flow: `tokio::spawn`'s handle
/// can be aborted, a `select!` can drop the losing branch, and a caller can
/// simply drop the future. In every one of those cases the population owner
/// stops running without ever reaching its own error handling, and the entry
/// stays `InFlight` with `population_owner = true` — so every later caller joins
/// as a waiter against an owner that no longer exists, waits out the full
/// timeout, and fails. One cancelled population made a key unusable until
/// something invalidated it.
///
/// A `Drop` guard is the only construct that runs on all of those paths. It is
/// deliberately *not* an abort of the fetch: the fetch belongs to the caller and
/// may be doing work the caller still wants. The guard only releases the control-
/// plane claim, which is this crate's to release.
struct PopulationGuard<'a> {
    cachelito: &'a Cachelito,
    key: Vec<u8>,
    armed: bool,
}

impl<'a> PopulationGuard<'a> {
    fn new(cachelito: &'a Cachelito, key: &[u8]) -> Self {
        Self {
            cachelito,
            key: key.to_vec(),
            armed: true,
        }
    }

    /// The population reached a terminal state; the guard has nothing to do.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PopulationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // `fail_with_error` is idempotent for this purpose: it clears the owner
        // flag and notifies waiters, and a missing entry is not an error worth
        // propagating from a destructor.
        let _ = self
            .cachelito
            .fail_with_error(&self.key, CacheError::Cancelled);
    }
}

/// Releases a commit intent if the write path stops before resolving it.
///
/// `prepare` puts the entry into `Prepared`, claims population ownership and
/// records an intent. Only `commit` or `abort` clears that. Without this guard a
/// cancelled `set` — dropped at any `.await` between `prepare` and the phase-3
/// `commit` — left the entry `Prepared` with `population_owner = true` and nobody
/// to satisfy it. Every later operation on that key then waited out the full
/// bound and failed, so one cancellation made a key unusable.
///
/// The same shape as [`PopulationGuard`], and for the same reason: `Drop` is the
/// only construct that runs on every exit path, including a dropped future.
///
/// Deliberately *not* a rollback of the data write. If `tier.set` completed and
/// the future died before `commit`, the value is on the rung and the intent is
/// gone; recovery cannot distinguish that from an abort.
///
/// This comment previously claimed the residue was unreachable because "the
/// tier read path consults the control plane first", which is what
/// `partial_commit_visible = false` rests on. That was true only while the entry
/// stayed `Prepared`. `abort` restored `Ready`, which is the thing that ended
/// `Prepared`, so on a previously-populated key the residue became reachable
/// again and the next read served it. So `abort` now leaves the entry unservable
/// (`Failed`) instead: one extra repopulation, not a wrong answer.
struct IntentGuard<'a> {
    cachelito: &'a Cachelito,
    token: CommitToken,
    armed: bool,
}

impl<'a> IntentGuard<'a> {
    fn new(cachelito: &'a Cachelito, token: CommitToken) -> Self {
        Self {
            cachelito,
            token,
            armed: true,
        }
    }

    fn token(&self) -> &CommitToken {
        &self.token
    }

    /// Abort the intent deliberately and disarm the guard.
    ///
    /// One call rather than `disarm()` followed by `abort(token())`: an earlier
    /// version removed the token from an `Option` on disarm, so that ordering
    /// panicked — and the test asserting cancellation safety is what caught it.
    /// Keeping the token and tracking liveness separately makes the mistake
    /// unrepresentable, and matches `PopulationGuard`'s shape.
    ///
    /// Conservative: the entry is left unservable rather than restored, because
    /// only the tier knows whether the write landed. Use
    /// [`Self::abort_proven_clean`] only for an error that proves otherwise.
    fn abort(&mut self, error: CacheError) {
        if !self.armed {
            return;
        }
        let _ = self.cachelito.abort(&self.token, error);
        self.armed = false;
    }

    /// Abort an intent the tier provably never wrote, restoring what it
    /// interrupted.
    ///
    /// Only for [`write_provably_stored_nothing`]. Anything else goes through
    /// [`Self::abort`], which assumes residue and leaves the entry unservable.
    fn abort_proven_clean(&mut self, error: CacheError) {
        if !self.armed {
            return;
        }
        let _ = self.cachelito.abort_proven_clean(&self.token, error);
        self.armed = false;
    }

    /// The intent reached a terminal state deliberately; the guard has nothing
    /// left to do. Used only on the commit-succeeded path.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for IntentGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // `abort` is idempotent, declines when a commit already won, and leaves
        // the entry unservable rather than restoring it — because a cancelled
        // write may already have landed on the rung, and a restored `Ready` would
        // serve it. See `Cachelito::abort`.
        let _ = self.cachelito.abort(&self.token, CacheError::Cancelled);
    }
}

/// Identity helper retained for clarity at the call site; `CacheError` is
/// `Copy`, so this simply returns the error for the control-plane marker.
fn error_kind(err: CacheError) -> CacheError {
    err
}

/// Whether this write error proves the tier never stored the value.
///
/// The whole recovery model turns on this. `CacheError` grew
/// `WriteIndeterminate` so a tier could say "my bytes may have landed", but
/// there is no matching variant for the converse — so nothing *else* may be read
/// as "they definitely did not". In particular these are all compatible with a
/// landed write:
///
/// * `TierUnavailable` — the rung's health flipped; the write may have been in
///   flight when it did. This is also what `FaultClass::WriteFailure` maps to in
///   the test harness, which makes it easy to mistake for a clean rejection.
/// * `Timeout` — we stopped waiting. The backend did not.
/// * `Corrupted` — a detection, not a rejection.
///
/// Only two are provable, and both are decided before any bytes are sent:
/// `SerializationFailed` (the value never encoded) and `CapacityExhausted`
/// (admission refused it). Everything else must abort conservatively.
fn write_provably_stored_nothing(error: CacheError) -> bool {
    matches!(
        error,
        CacheError::SerializationFailed | CacheError::CapacityExhausted
    )
}

/// The rung below `rung`, or `L6` when there is none.
///
/// Written as a free function so the ladder walk has one definition, and so
/// "below L0" lands on the authority rung — which is then rejected by the
/// `is_cache_rung` guard rather than wrapping around to a rung the caller did
/// not choose.
fn previous_rung(rung: TierId) -> TierId {
    match rung.as_usize() {
        0 => TierId::L6,
        n => TierId::from_usize(n - 1).unwrap_or(TierId::L0),
    }
}

impl<K, V, P> CacheManager<K, V, P> {
    /// How many times a single write may be re-prepared after losing a commit
    /// race on its *own* rung.
    ///
    /// This budget is per-rung and is spent only on lost races. It used to also
    /// bound the rung walk, which is what let a benign race end a write: a
    /// loser that spent its two attempts descended, and descending after losing
    /// is wrong in its own right (see the race arm of `set`).
    ///
    /// The value is generous on purpose. Exhausting it requires losing this many
    /// races in a row on one rung, which means the key is being rewritten
    /// continuously by writers that never pause; a cache write that hits that
    /// is a caller-visible condition worth reporting, not something to hide
    /// behind an unbounded loop that would livelock the executor.
    const MAX_COMMIT_ATTEMPTS: usize = 64;

    /// How many times one write may reclaim a slot before giving up (B17).
    ///
    /// Each pass evicts at most one entry and then re-walks the ladder, so this
    /// is an upper bound on the work a single `set` can do against a saturated
    /// ladder. It is deliberately generous: in practice a pass frees the slot
    /// the very next rung needs and the write lands. The bound exists so a
    /// ladder whose entries are all unevictable -- every one `InFlight` -- makes
    /// progress to a refusal instead of spinning.
    const MAX_EVICTION_PASSES: usize = 16;

    /// How many times `get_or_fetch` may hand the entry to another caller before
    /// giving up.
    ///
    /// A loop rather than recursion, and the reason is not style: the recursive
    /// form made the future's own type infinitely sized, because the re-entry is
    /// generic over `F` and `Fut` and so is a strictly larger instantiation of the
    /// same future.
    const MAX_POPULATION_ATTEMPTS: usize = 8;
}

/// The rung reported for an operation that never selected one.
///
/// An unauthenticated request is refused before policy selection, so there is no
/// rung to name. L0 is the honest answer: it is the base rung, and it is
/// explicitly *not* a claim that the operation reached it.
const fn decision_tier_placeholder() -> TierId {
    TierId::L0
}

/// Unreachable marker: `get_or_fetch`'s retry loop always returns from inside the
/// loop body. Kept as a function so the loop's exit path has a type without an
/// `unreachable!()` that a future edit could turn into a live panic.
const fn attempt_unreachable() -> bool {
    true
}

pub struct CacheManager<K, V, P> {
    policy: P,
    cachelito: Cachelito,
    tier_registry: TierRegistry,
    tiers: Vec<Arc<dyn CacheTier<V>>>,
    _pool: MemoryPool<V>,
    wait_timeout: Duration,
    /// Which rung is the system of record.
    ///
    /// Explicit configuration, not a tier number. The previous code compared
    /// against `TierId::L6` in one place and against `idx == TierId::L6.as_usize()`
    /// in another, which misfired whenever the caller's tier vector was not in
    /// ladder order.
    authority_tier: TierId,
    /// Where operation records go. Injected rather than global: a global sink is
    /// a process-wide mutable that tests cannot isolate, and observability that
    /// cannot be isolated cannot be asserted on.
    telemetry: SharedSink,
    _key: PhantomData<K>,
}

/// Whether a returned value was current or a fallback.
///
/// This exists because "serve the stale value when revalidation fails" and
/// "silently pass stale data off as fresh" are the same bytes on the wire and
/// opposite promises. `cia.integrity.silent_stale_data_acceptance = false`
/// was a claim the crate could not honour while `refresh` returned a bare
/// `Option<V>`: a caller holding `Some(old)` after a failed revalidation had
/// no way to learn that, so the distinction existed nowhere to be observed.
///
/// `Stale` means *this value may be out of date and was served because
/// something better could not be produced right now*. It is not an error and
/// not a corruption report; it is a caveat the caller is now obliged to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Produced by the fetch, or the current committed value.
    Fresh,
    /// A previously committed value, served because revalidation failed or
    /// was already in progress.
    Stale,
}

/// A value plus whether it is a fallback.
///
/// A `None` value is never stale: staleness is a property of a value that was
/// returned, so `freshness` is only meaningful when `value` is `Some`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup<V> {
    /// The value read, or `None` for a miss.
    pub value: Option<V>,
    /// Whether `value` is a stale fallback.
    pub freshness: Freshness,
}

impl<K, V, P> CacheManager<K, V, P>
where
    K: Key + Send + Sync,
    V: Clone + Send + Sync + 'static,
    P: CachePolicy<K, V>,
{
    pub fn new(
        policy: P,
        cachelito: Cachelito,
        tier_registry: TierRegistry,
        tiers: Vec<Arc<dyn CacheTier<V>>>,
        pool: MemoryPool<V>,
    ) -> Self {
        Self::with_timeout(
            policy,
            cachelito,
            tier_registry,
            tiers,
            pool,
            Duration::from_secs(5),
        )
    }

    pub fn with_timeout(
        policy: P,
        cachelito: Cachelito,
        tier_registry: TierRegistry,
        tiers: Vec<Arc<dyn CacheTier<V>>>,
        pool: MemoryPool<V>,
        wait_timeout: Duration,
    ) -> Self {
        CacheManager {
            policy,
            cachelito,
            tier_registry,
            tiers,
            _pool: pool,
            wait_timeout,
            authority_tier: crate::policy::AUTHORITY_TIER,
            telemetry: std::sync::Arc::new(crate::telemetry::NoTelemetry),
            _key: PhantomData,
        }
    }

    /// The same manager with a different authority rung.
    ///
    /// Authority is a configured role, so it is set here rather than inferred.
    /// Passing a rung outside the ladder is rejected: making a cache rung
    /// authoritative is how a fallback ends up serving as the system of record,
    /// and it should be a construction-time error rather than a runtime surprise.
    #[must_use]
    pub fn with_authority(mut self, authority: TierId) -> Self {
        assert!(
            authority.is_authority_rung(),
            "authority must be the authority rung; {authority} is a cache rung"
        );
        self.authority_tier = authority;
        self
    }

    /// The same manager with a telemetry sink.
    ///
    /// Consuming, like the other builders. A `&mut self` variant would let a
    /// caller swap the sink between operations, which makes the records a
    /// capture of nothing in particular.
    #[must_use]
    pub fn with_telemetry(mut self, sink: SharedSink) -> Self {
        self.telemetry = sink;
        self
    }

    /// Rebuild this manager around a different sink.
    ///
    /// Exists so a manager already held in an `Arc` — which is how the async API
    /// is actually used — can be given telemetry without every test having to
    /// construct the whole stack by hand.
    #[must_use]
    pub fn with_shared_telemetry(self, sink: SharedSink) -> Self {
        self.with_telemetry(sink)
    }

    /// Which rung is the system of record.
    #[must_use]
    pub const fn authority_tier(&self) -> TierId {
        self.authority_tier
    }

    /// Emit one operation record.
    fn emit(&self, record: OperationRecord) {
        self.telemetry.record(&record);
    }

    /// Run a data-plane read and record it.
    ///
    /// Every read goes through here rather than calling `tier.get` directly, so
    /// there is exactly one place where a record is produced. Instrumenting the
    /// tier calls individually would leave a path uncovered the next time someone
    /// adds an operation, and an unrecorded operation is invisible in exactly the
    /// situation you want to see it: a failure.
    async fn observed_read(
        &self,
        op: Operation,
        key_ref: &KeyRef<'_>,
        tier_id: TierId,
        fut: impl std::future::Future<Output = Result<Option<V>, CacheError>>,
    ) -> Result<Option<V>, CacheError> {
        let started = std::time::Instant::now();
        let result = self.bounded(fut).await;
        let outcome = match &result {
            Ok(Some(_)) => crate::telemetry::Outcome::Served,
            Ok(None) => crate::telemetry::Outcome::Miss,
            Err(e) => crate::telemetry::Outcome::from_error(*e),
        };
        let mut record = OperationRecord::begin(op, key_ref)
            .with_source(tier_id)
            .with_backend(self.backend_of(tier_id))
            .with_generation(Generation::new(0))
            .finish(started, outcome);
        if let Err(e) = result {
            record = record.with_failure(e);
        }
        self.emit(record);
        result
    }

    /// Run a data-plane write and record it.
    async fn observed_write(
        &self,
        op: Operation,
        key_ref: &KeyRef<'_>,
        tier_id: TierId,
        fut: impl std::future::Future<Output = Result<(), CacheError>>,
    ) -> Result<(), CacheError> {
        let started = std::time::Instant::now();
        let result = self.bounded(fut).await;
        let outcome = match &result {
            Ok(()) => crate::telemetry::Outcome::Populated,
            Err(e) => crate::telemetry::Outcome::from_error(*e),
        };
        let mut record = OperationRecord::begin(op, key_ref)
            .with_destination(tier_id)
            .with_backend(self.backend_of(tier_id))
            .finish(started, outcome);
        if let Err(e) = result {
            record = record.with_failure(e);
        }
        self.emit(record);
        result
    }

    /// Record an operation that never reached a tier: a denial, or a miss decided
    /// by the control plane alone.
    fn observed_control_only(
        &self,
        op: Operation,
        key_ref: &KeyRef<'_>,
        tier_id: TierId,
        started: std::time::Instant,
        outcome: crate::telemetry::Outcome,
    ) {
        self.emit(
            OperationRecord::begin(op, key_ref)
                .with_source(tier_id)
                .finish(started, outcome),
        );
    }

    /// Record that a fail-open scan found a value on another rung.
    fn record_fallback(&self, key_ref: &KeyRef<'_>, tier: TierId, cause: CacheError) {
        let started = std::time::Instant::now();
        self.emit(
            OperationRecord::begin(Operation::GetOrFetch, key_ref)
                .with_source(tier)
                .with_backend(self.backend_of(tier))
                .with_fallback(true)
                .with_failure(cause)
                .finish(started, crate::telemetry::Outcome::FellBack),
        );
    }

    /// Record that a refresh fell back to serving the stale value.
    fn record_telemetry_refresh_failure(
        &self,
        key_ref: &KeyRef<'_>,
        snapshot: &ControlSnapshot,
        cause: CacheError,
    ) {
        let started = std::time::Instant::now();
        self.emit(
            OperationRecord::begin(Operation::Refresh, key_ref)
                .with_source(snapshot.tier)
                .with_generation(snapshot.generation)
                .with_failure(cause)
                .finish(started, crate::telemetry::Outcome::FellBack),
        );
    }

    /// Every rung's continuity state, as a consumer sees it.
    ///
    /// Derived from the same counters the circuit breaker uses, so the report
    /// cannot disagree with the routing decision that produced it.
    #[must_use]
    pub fn continuity(&self) -> crate::continuity::ContinuityReport {
        let mut states = std::collections::BTreeMap::new();
        for &tier in self.tier_registry.all() {
            states.insert(tier, self.continuity_of(tier));
        }
        crate::continuity::ContinuityReport { states }
    }

    fn continuity_of(&self, tier: TierId) -> crate::continuity::ContinuityState {
        use crate::continuity::ContinuityState;
        if !self.has_tier(&tier) {
            return ContinuityState::Unavailable;
        }
        let health = self.tier_registry.tier_health(tier);
        match health.consecutive_failures {
            0 => ContinuityState::Healthy,
            n if n >= crate::continuity::FAILURE_THRESHOLD => ContinuityState::Unavailable,
            _ => ContinuityState::Degraded,
        }
    }

    /// Resolve every commit intent older than `INTENT_RECOVERY_AGE`.
    ///
    /// Idempotent by construction: a `Write` intent aborts, which is safe to
    /// repeat, and a `Move` intent is left for an external reconciler, which is
    /// also safe to repeat because it changes nothing. The two are reported
    /// separately, so `recovery_failure_must_be_observable` is satisfied by
    /// construction rather than by a log line nobody reads.
    pub fn recover(&self) -> RecoveryReport {
        self.recover_older_than(crate::continuity::INTENT_RECOVERY_AGE)
    }

    /// As [`Self::recover`], with an explicit age threshold.
    pub fn recover_older_than(&self, age: Duration) -> RecoveryReport {
        let mut report = RecoveryReport::default();
        for (address, intent) in self.cachelito.stale_intents(age.as_nanos() as u64) {
            match intent.kind {
                // A write's value is reproducible, so aborting is both safe and
                // complete. Aborting needs only the key hash, which is all the
                // control plane keeps.
                IntentKind::Write => {
                    // Only count what was actually cleared. Two sweeps racing the
                    // same intent must between them count it once, or the pass
                    // reports more recoveries than there were intents.
                    match self
                        .cachelito
                        .abort_intent_by_address(address, CacheError::UncommittedIntent)
                    {
                        Ok(true) => report.recovered += 1,
                        Ok(false) => {}
                        Err(_) => report.failed += 1,
                    }
                }
                // A move cannot be resolved by this process, and must not be
                // guessed at.
                //
                // Completing it forward means reading the source rung and writing
                // the destination, which needs the key bytes; the control plane
                // keeps only a hash. Aborting is not a safe substitute either: the
                // move removes the source before it commits, so a crash in that
                // window leaves the value only at the destination, and aborting
                // would point the control plane at a source that is now empty.
                //
                // So the intent is counted as needing reconciliation and left
                // exactly as it is. Reported separately from `failed` because it is
                // not a sweep that tried and lost — it is a job this sweep does not
                // have the information to do, and folding the two together would
                // make a permanently stuck key look like ordinary contention.
                //
                // The claim is nevertheless released (B15). Leaving it held
                // wedged the key's population path permanently: `acquire`
                // declines while an owner is set, so nothing could ever write
                // or populate it again, and only an external reconciler that
                // never arrived could clear it.
                //
                // Releasing is safe precisely because this does *not* resolve
                // the move. The entry is marked `Failed` with its generation
                // advanced and its intent cleared, so nothing points at the
                // emptied source: reads report a miss and a later write
                // supersedes whatever the interrupted move left behind. For a
                // cache entry that is recoverable; a permanently unusable key is
                // not. The obligation stays visible in `needs_reconciliation`,
                // so releasing the wedge never hides it.
                IntentKind::Move => {
                    report.needs_reconciliation += 1;
                    match self.cachelito.release_population_claim(address) {
                        Ok(true) => report.released_for_reconciliation += 1,
                        Ok(false) => {}
                        Err(_) => report.failed += 1,
                    }
                }
            }
        }
        report
    }

    pub fn cachelito(&self) -> &Cachelito {
        &self.cachelito
    }

    /// Run a data-plane operation under the manager's wait bound.
    ///
    /// The control plane is unaffected either way — that is the whole
    /// control/data split — but an unbounded read means one hung rung can pin a
    /// caller's task forever, which is `unbounded_queue_growth` in practice and
    /// leaves no signal that anything went wrong. Population already had this
    /// bound; reads did not.
    async fn bounded<T, F>(&self, fut: F) -> Result<T, CacheError>
    where
        F: std::future::Future<Output = Result<T, CacheError>>,
    {
        match tokio::time::timeout(self.wait_timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(CacheError::Timeout),
        }
    }

    /// Encode `key` under `ctx`'s tenant.
    ///
    /// The tenant is part of the key, not metadata beside it. `IdentityContext`
    /// carried a `tenant` field that no line of the crate read, so two tenants
    /// using the same application key shared one entry and one control-plane
    /// slot — a cross-tenant read that the authn/authz gates could not prevent,
    /// because by the time they ran there was nothing left to separate.
    ///
    /// Framing the tenant into the key is the only fix that holds for a caller
    /// who authenticates as one tenant and reads another's key: the entry is
    /// simply not there. See `src/key::frame_tenant_key` for why the frame uses
    /// an explicit separator rather than concatenation.
    fn encode_key<'a>(
        key: &K,
        ctx: &CacheContext,
        buf: &'a mut [u8; MAX_KEY_SIZE],
    ) -> Result<KeyRef<'a>, CacheError> {
        let mut raw = [0u8; MAX_KEY_SIZE];
        let key_len = key.encode(&mut raw)?;
        let len = crate::key::frame_tenant_key(&ctx.identity().tenant, &raw[..key_len], buf)?;
        Ok(KeyRef(&buf[..len]))
    }

    /// Authentication gate: reject unauthenticated requests before any
    /// state is touched (spec: `authn_gate = "pre-policy"`).
    fn check_auth(ctx: &CacheContext) -> Result<(), CacheError> {
        if !ctx.is_authenticated() {
            return Err(CacheError::Unauthenticated);
        }
        Ok(())
    }

    /// Resolve the policy decision for an operation using the caller's context,
    /// a fresh (absent) cache state, and no value.
    /// Select from a fresh (absent) cache state.
    fn resolve(
        &self,
        request: &CacheRequest<K, V>,
        ctx: &CacheContext,
    ) -> crate::policy::PolicyDecision {
        let state = CacheState::new();
        self.policy.select(request, &state, ctx.identity())
    }

    /// Select against a real snapshot from the control plane (populate/hit paths).
    fn resolve_with_snapshot(
        &self,
        request: &CacheRequest<K, V>,
        ctx: &CacheContext,
        snapshot: &ControlSnapshot,
    ) -> crate::policy::PolicyDecision {
        let health = self.tier_registry.tier_health(snapshot.tier);
        let state = CacheState::from_snapshot(snapshot, health);
        self.policy.select(request, &state, ctx.identity())
    }

    /// Build the operation's `CacheRequest` once (single key clone), reusing it
    /// for every policy resolution in the operation.
    fn request_for(op: CacheOperation, key: &K, ctx: &CacheContext) -> CacheRequest<K, V> {
        let mut request = CacheRequest::new(op, key.clone());
        if let Some(ttl) = ctx.ttl() {
            request = request.with_ttl(ttl);
        }
        request
    }

    fn authorize(decision: &crate::policy::PolicyDecision) -> Result<(), CacheError> {
        if decision.authorized {
            Ok(())
        } else {
            Err(CacheError::Unauthorized)
        }
    }

    /// Record the outcome of a call against a tier so the circuit breaker
    /// can trip/recover based on real traffic.
    fn record_outcome(&self, tier: TierId, result: Result<(), CacheError>) {
        match result {
            Ok(()) => self.tier_registry.recover(tier),
            Err(CacheError::TierUnavailable) => self.tier_registry.fail(tier),
            Err(_) => {}
        }
    }

    /// Record a tier read where the payload detail is not needed.
    fn record_read<T>(&self, tier: TierId, result: &Result<T, CacheError>) {
        match result {
            Ok(_) => self.tier_registry.recover(tier),
            Err(CacheError::TierUnavailable) => self.tier_registry.fail(tier),
            Err(_) => {}
        }
    }

    pub async fn get(&self, key: &K, ctx: &CacheContext) -> Result<Option<V>, CacheError> {
        let started = std::time::Instant::now();
        let mut buf = [0u8; MAX_KEY_SIZE];
        // Encode before the authn gate so a refused request can still be recorded
        // against a key identity. The identity is a digest, so this leaks nothing,
        // and a denial that leaves no trace is a denial nobody can audit.
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;

        if let Err(e) = Self::check_auth(ctx) {
            self.observed_control_only(
                Operation::Get,
                &key_ref,
                decision_tier_placeholder(),
                started,
                crate::telemetry::Outcome::from_error(e),
            );
            return Err(e);
        }

        let request = Self::request_for(CacheOperation::Get, key, ctx);
        let decision = self.resolve(&request, ctx);
        if let Err(e) = Self::authorize(&decision) {
            self.observed_control_only(
                Operation::Get,
                &key_ref,
                decision.tier,
                started,
                crate::telemetry::Outcome::from_error(e),
            );
            return Err(e);
        }

        // `peek`, never `acquire`. `get` is a pure read: it never populates, so
        // taking population ownership was always wrong — and observably so. A
        // read against a failing rung claimed the entry, propagated the error,
        // and left it `InFlight` with an owner nobody would ever satisfy, so
        // every subsequent read joined as a waiter and waited out the timeout.
        // One failing read turned one key into a permanently broken one.
        let snapshot = self.cachelito.peek(key_ref.0)?;

        match snapshot.state {
            EntryState::Ready if !snapshot.expired => {
                let Some(tier) = self.bound_tier(&snapshot.tier) else {
                    let e = CacheError::TierUnavailable;
                    self.observed_control_only(
                        Operation::Get,
                        &key_ref,
                        snapshot.tier,
                        started,
                        crate::telemetry::Outcome::from_error(e),
                    );
                    return Err(e);
                };
                let result = self
                    .observed_read(Operation::Get, &key_ref, snapshot.tier, tier.get(&key_ref))
                    .await;
                self.record_read(snapshot.tier, &result);
                // Revalidate across the await (B19). `peek` observed `Ready` and
                // this read is the one unguarded window in `get`: an `abort`
                // landing while we are parked on the rung leaves the entry
                // `Failed` with the generation advanced, and the rung still
                // holds the residue of the write that was abandoned. Without
                // this the caller is handed a value the control plane has
                // stopped describing.
                //
                // Only the *state* is rechecked, not the generation. A benign
                // concurrent commit does not disown the entry -- the value is
                // still one this cache committed -- whereas `Failed` means the
                // value is residue of an abandoned intent and must not be
                // served. Rechecking state keeps this consistent with the
                // `InFlight` arm above, which also reports `Miss` once the
                // settled entry is not `Ready`.
                if result.is_ok()
                    && let Ok(after) = self.cachelito.peek(key_ref.0)
                    && !matches!(after.state, EntryState::Ready)
                {
                    return Err(CacheError::Miss);
                }
                result
            }
            EntryState::Ready => {
                // TTL elapsed: mark Stale and report a miss (lazy expiry). This
                // is the only mutation `get` performs, and it is a state
                // transition on an existing entry, not a claim.
                let _ = self.cachelito.set_state(key_ref.0, EntryState::Stale);
                self.observed_control_only(
                    Operation::Get,
                    &key_ref,
                    snapshot.tier,
                    started,
                    crate::telemetry::Outcome::Miss,
                );
                Err(CacheError::Miss)
            }
            // A population or a commit is under way. Wait for it rather than
            // reading a rung that is mid-write, then re-observe so the rung and
            // generation come from the settled entry rather than the stale view.
            EntryState::InFlight | EntryState::Prepared if !snapshot.population_owner => {
                self.wait_for_population(&key_ref, &snapshot).await?;
                let after = self.cachelito.peek(key_ref.0)?;
                if let Some(err) = after.last_error {
                    return Err(err);
                }
                if after.state != EntryState::Ready {
                    return Err(CacheError::Miss);
                }
                let Some(tier) = self.bound_tier(&after.tier) else {
                    return Err(CacheError::TierUnavailable);
                };
                let result = self
                    .observed_read(Operation::Get, &key_ref, after.tier, tier.get(&key_ref))
                    .await;
                self.record_read(after.tier, &result);
                result
            }
            _ => {
                self.observed_control_only(
                    Operation::Get,
                    &key_ref,
                    snapshot.tier,
                    started,
                    crate::telemetry::Outcome::Miss,
                );
                Err(CacheError::Miss)
            }
        }
    }

    pub async fn get_or_fetch<F, Fut>(
        &self,
        key: &K,
        ctx: &CacheContext,
        fetch: F,
    ) -> Result<V, CacheError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<V, CacheError>>,
    {
        Self::check_auth(ctx)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;

        let request = Self::request_for(CacheOperation::Get, key, ctx);
        let decision = self.resolve(&request, ctx);
        Self::authorize(&decision)?;

        // Each iteration requires another caller to have *finished* a commit, not
        // merely to have been scheduled. Exceeding the bound means commits are
        // completing faster than we can observe them, which is contention, not a
        // livelock to keep retrying through.
        for _attempt in 0..Self::MAX_POPULATION_ATTEMPTS {
            let snapshot = self.cachelito.acquire(key_ref.0, decision.tier)?;

            // A Ready entry whose TTL has elapsed must be re-claimed before it can
            // be republished, and `Stale` is claimable while `Ready` is not.
            let snapshot = if snapshot.state == EntryState::Ready && snapshot.expired {
                let _ = self.cachelito.set_state(key_ref.0, EntryState::Stale);
                self.cachelito.acquire(key_ref.0, decision.tier)?
            } else {
                snapshot
            };

            // Fast path: a committed, unexpired value. A tier that *errors* is a
            // real failure and propagates; a tier that *misses* falls through to
            // population, because the control plane and the data plane can
            // disagree (evicted payload, expired-in-the-tier-only entry) and a
            // miss is not an error.
            //
            // Falling through rather than returning is also what keeps
            // single-flight intact for a Ready-but-empty entry: the re-acquire
            // above either granted ownership or joined the existing population.
            if snapshot.state == EntryState::Ready && !snapshot.expired {
                let Some(tier) = self.bound_tier(&snapshot.tier) else {
                    return Err(CacheError::TierUnavailable);
                };
                match self
                    .observed_read(
                        Operation::GetOrFetch,
                        &key_ref,
                        snapshot.tier,
                        tier.get(&key_ref),
                    )
                    .await
                {
                    Ok(Some(v)) => return Ok(v),
                    Ok(None) => {}
                    Err(e) => return Err(e),
                }
            }

            match snapshot.state {
                // Someone else owns the population. This is the single-flight
                // join point, and the only place waiters are created.
                EntryState::InFlight if !snapshot.population_owner => {
                    return self.wait_and_get(key, ctx, &snapshot).await;
                }
                // A commit we own is under way. A read must not observe an
                // uncommitted write and must not race it, so wait for it to settle
                // and then re-observe. This is the one arm that re-enters the loop.
                EntryState::Prepared if snapshot.population_owner => {
                    self.wait_for_population(&key_ref, &snapshot).await?;
                    continue;
                }
                EntryState::Prepared => {
                    return self.wait_and_get(key, ctx, &snapshot).await;
                }
                // We hold population ownership (or nothing was in flight).
                _ => {
                    return self
                        .become_population_owner(
                            key,
                            key_ref,
                            snapshot.generation,
                            ctx,
                            &decision,
                            fetch,
                        )
                        .await;
                }
            }
        }
        // The loop above returns on every path, so reaching here means every
        // iteration handed the entry to another caller. Report it as a timeout
        // rather than panicking: the condition is contention, and a library that
        // panics under contention is worse than one that reports it.
        let _ = attempt_unreachable();
        Err(CacheError::Timeout)
    }

    pub async fn set(&self, key: &K, value: V, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;

        // Observe without claiming. A write does not need population ownership:
        // it is not populating, and taking it wedged the entry whenever the write
        // then failed.
        let snapshot = self.cachelito.peek(key_ref.0)?;
        let request = Self::request_for(CacheOperation::Set, key, ctx);
        let decision = self.resolve_with_snapshot(&request, ctx, &snapshot);
        Self::authorize(&decision)?;

        // Authority rule (tripwired by `put_never_writes_l6_authority`): L6 is
        // the judge, not a rung. A blind put would write a projection into the
        // authority and let a cache tier decide what is true. Authority writes
        // go through the owning repository, which then invalidates downward.
        if decision.tier == self.authority_tier {
            return Err(CacheError::PolicyDenied);
        }

        // Walk down the ladder from the policy's choice.
        //
        // Two conditions force the walk, and both were previously reported to the
        // caller as failures:
        //
        // * **Capacity.** A rung is fixed-capacity by design, so a cache that
        //   hard-fails once its rung is full is not a cache — it is a map that
        //   stops working. The whole point of the ladder is that a lower rung has
        //   room, so a full rung is a routing decision, not an error.
        // * **Availability.** The same argument for a rung that is merely down.
        //   A write must not fail because the rung policy preferred happens to be
        //   unavailable when a healthy one sits below it.
        // * **A lost race.** Concurrent writers on one key both prepare; one
        //   commits and the other's token is refused. That is correct
        //   optimistic concurrency, but surfacing it makes an ordinary write look
        //   like a failure, so the loser re-reads and tries again.
        let mut last_error = CacheError::TierUnavailable;
        // Two passes: the first walks the ladder, the second runs after
        // eviction has freed a slot. Eviction is the *last resort*, not the
        // first move (B17) -- degrading preserves a hotter rung's contents,
        // whereas evicting at the first full rung throws them away to make room
        // for a colder key. That is a worse cache, not a better one.
        // Eviction is attempted after *every* exhausted walk, not only the
        // first. Gating it to `pass == 0` looked like a bound and was not: the
        // first eviction advanced the pass, and every later exhaustion then
        // skipped the reclaim and refused the write -- so the ladder still ran
        // out, once. The bound that matters is how much work one write may do,
        // which is MAX_EVICTION_PASSES below.
        'pass: for _pass in 0..=Self::MAX_EVICTION_PASSES {
            let mut rung = decision.tier;

            'outer: while rung.is_cache_rung() {
                let Some(tier) = self.bound_tier(&rung) else {
                    rung = previous_rung(rung);
                    continue;
                };

                let mut lost_races = 0usize;
                loop {
                    // Phase 1: record the intent. From here the entry reads as a miss,
                    // so a crash between the phases cannot expose a half-written
                    // value.
                    let token = self
                        .cachelito
                        .prepare(key_ref.0, None, rung, IntentKind::Write)?;

                    // From here the intent is owned by the guard, not by control
                    // flow. Every exit below either disarms it explicitly or lets
                    // `Drop` abort it, so a cancelled `set` cannot leave the key
                    // `Prepared` with an owner nobody will satisfy.
                    let mut intent = IntentGuard::new(&self.cachelito, token);

                    // Phase 2: the data write, outside every guard.
                    let set_result = self
                        .observed_write(
                            Operation::Set,
                            &key_ref,
                            rung,
                            tier.set(&key_ref, value.clone(), ctx.ttl()),
                        )
                        .await;
                    self.record_outcome(rung, set_result);

                    // A failed write is ambiguous unless the tier says otherwise.
                    //
                    // The commit-failure path below already removes what it created; the
                    // error path did not, so a backend that stored the bytes and then
                    // reported failure left the control plane saying "aborted" and the
                    // rung holding a value nobody authorised. Because `abort` restores
                    // the previous state, that residue was not merely an orphan: on a
                    // previously-populated key the rung had silently become the *new*
                    // value while the control plane still described the old one, which
                    // is `partial_commit_visible = true` reached by accident.
                    //
                    // Compensating unconditionally would be worse, and an earlier version
                    // of this did exactly that, gated on a per-tier capability flag — which
                    // broke `write_failure_fails_a_write_and_leaves_the_old_value`. A tier
                    // that rejects a write provably stored nothing, so the rung still holds
                    // the *previous* value, and removing that destroys a good value in the
                    // name of clearing residue that does not exist.
                    //
                    // The discriminator is `WriteIndeterminate`, and it has to be: only the
                    // tier knows whether its bytes landed. A capability flag is a
                    // per-tier blanket claim and cannot distinguish a rejected write from
                    // an accepted one, so it over-removes. `ATOMIC_WRITE_OR_ERROR` remains
                    // as reported capability — it is true and a consumer may want it — but
                    // it does not govern this decision.
                    if let Err(CacheError::WriteIndeterminate) = set_result {
                        let _ = self.bounded(tier.remove(&key_ref)).await;
                    }

                    match set_result {
                        Ok(()) => {
                            // Phase 3: commit. `commit` re-checks the generation, so an
                            // invalidation that landed during the await wins and this
                            // write is rejected rather than silently undoing it.
                            return match self.cachelito.commit(intent.token(), ctx.ttl()) {
                                Ok(()) => {
                                    intent.disarm();
                                    Ok(())
                                }
                                // A lost race is not an error, and it is not a
                                // reason to change rung. The winner's value is
                                // *newer* than this one, so the response is to
                                // re-prepare against the generation it left behind
                                // and try again right here.
                                //
                                // This arm used to fall out of a bounded retry loop
                                // that, when exhausted, set `last_error =
                                // StaleGeneration` and walked down the ladder. That
                                // was wrong twice over: it wrote a losing, older
                                // value into a colder tier, and because the walk
                                // *consumed* the budget it could run out of rungs
                                // and hand the caller a control-plane error from a
                                // public API documented never to do so.
                                Err(CacheError::StaleGeneration) => {
                                    lost_races += 1;
                                    if lost_races >= Self::MAX_COMMIT_ATTEMPTS {
                                        let e = CacheError::WriteContended;
                                        // The token is already dead, so the guard
                                        // aborts it as we release. Nothing was
                                        // committed at this rung by this attempt.
                                        drop(intent);
                                        return Err(e);
                                    }
                                    // Yield rather than spin: the writer we lost
                                    // to needs the executor to make progress too.
                                    tokio::task::yield_now().await;
                                    continue;
                                }
                                Err(e) => {
                                    // Uncommitted: the rung holds residue this write
                                    // created, so remove it before releasing the intent.
                                    let _ = tier.remove(&key_ref).await;
                                    intent.abort(e);
                                    Err(e)
                                }
                            };
                        }
                        // This rung cannot take the write. Abort cleanly, then try the
                        // next one down.
                        //
                        // Any rung-level failure forces the walk, because each one means
                        // *this* rung cannot serve and a lower one might: full,
                        // unavailable, slow enough to hit the bound, or holding a
                        // record it cannot vouch for. Degrading only on capacity left a
                        // write failing outright whenever the preferred rung was merely
                        // down or slow, which is exactly the single-tier cascade the
                        // contract forbids. If every rung below refuses, the walk ends
                        // and the remembered error is returned, so nothing is masked.
                        //
                        // Errors that indicate a caller or contract problem rather than a
                        // rung problem — `ConfigurationError`, `Miss` — return
                        // immediately, because a lower rung cannot fix them.
                        Err(
                            e @ (CacheError::CapacityExhausted
                            | CacheError::TierUnavailable
                            | CacheError::Timeout
                            | CacheError::Corrupted
                            | CacheError::SerializationFailed
                            | CacheError::WriteIndeterminate),
                        ) => {
                            // Only an error that proves nothing was written may
                            // restore the interrupted state. The rest leave the entry
                            // unservable: `TierUnavailable` and `Timeout` in this very
                            // arm are both compatible with a write that landed, and
                            // restoring `Ready` over a rung of unknown contents is how
                            // an uncommitted value became readable.
                            if write_provably_stored_nothing(e) {
                                intent.abort_proven_clean(e);
                            } else {
                                intent.abort(e);
                            }
                            last_error = e;
                            rung = previous_rung(rung);
                            continue 'outer;
                        }
                        Err(e) => {
                            // Undo phase 1 so the entry is claimable again, and report
                            // the data-plane error rather than the bookkeeping one.
                            if write_provably_stored_nothing(e) {
                                intent.abort_proven_clean(e);
                            } else {
                                intent.abort(e);
                            }
                            return Err(e);
                        }
                    }
                }
                // Unreachable: the inner loop only ever exits by returning (every
                // path out of it is a `return` or a `continue 'outer`), so there is
                // no longer a way to exhaust it and fall through to a rung change.
                // This is the line that used to turn a lost race into a rung
                // descent; its absence is the fix for D1.
            }

            // The ladder walk is exhausted. Only now is eviction worth its cost.
            if last_error == CacheError::CapacityExhausted
                && self.try_free_somewhere_in_the_ladder(key_ref.0)
            {
                // A slot exists somewhere now; walk the ladder again and take it.
                continue 'pass;
            }
            break;
        }

        Err(last_error)
    }

    /// Reclaim one slot anywhere in the ladder, coldest rung first.
    ///
    /// Coldest-first because that is where the ladder walk was heading, so those
    /// are the least valuable entries to lose. An earlier version reversed the
    /// order and evicted from L1: the hottest rung then kept accepting every
    /// write, the ladder never degraded at all, and the soak test asserting
    /// degradation failed -- which is how the mistake surfaced.
    ///
    /// Returns whether anything was actually freed. `false` means every bound
    /// rung's candidates were refused (all `InFlight`, or a tier that does not
    /// implement eviction), and the caller should stop and report rather than
    /// keep asking.
    fn try_free_somewhere_in_the_ladder(&self, key: &[u8]) -> bool {
        let mut budget = 0usize;
        for candidate in crate::policy::cache_ladder() {
            let Some(tier) = self.bound_tier(&candidate) else {
                continue;
            };
            if self.try_evict_at_rung(&tier, key, &mut budget) {
                return true;
            }
        }
        false
    }

    /// Free one slot at `tier` by evicting an entry the control plane agrees is
    /// evictable. Returns whether a slot was actually reclaimed.
    ///
    /// The two-phase shape is the safety property (B17). The tier only
    /// *nominates* an address; the control plane *authorises* it, atomically,
    /// by checking admissibility and advancing the generation. Only then is the
    /// slot removed, and conditionally on the address still being there.
    ///
    /// Why the generation bump matters: without it, a writer between `prepare`
    /// and `commit` could commit into a slot that eviction is about to reuse.
    /// The commit checks `token.expired_by(entry.generation())`; bumping first
    /// makes that true, so the stranded write is refused rather than landing in
    /// a slot that now describes a different key. Without this the tier would
    /// have to evict on trust, and the result is a silently stale value -- the
    /// exact defect B19 was promoted for.
    ///
    /// `attempts` is bounded by the caller across the whole write. A rung whose
    /// every entry is unevictable (all `InFlight`, or all owned by populations)
    /// must not spin: the walk degrades instead.
    fn try_evict_at_rung(
        &self,
        tier: &Arc<dyn CacheTier<V>>,
        key: &[u8],
        attempts: &mut usize,
    ) -> bool {
        const MAX_EVICTION_ATTEMPTS: usize = 8;

        while *attempts < MAX_EVICTION_ATTEMPTS {
            *attempts += 1;
            // Phase 1: the tier nominates. Default `None` means this tier does
            // not support eviction at all, so we stop immediately rather than
            // pretend.
            let Some(address) = tier.eviction_candidate() else {
                return false;
            };

            // Phase 2: the control plane disposes. Synchronous, so no shard
            // guard is held across the await below.
            match self.cachelito.reserve_eviction(address) {
                Ok(true) => {}
                // The nominated entry became ineligible (a population claimed
                // it, a commit intent appeared). Refusal is a normal outcome;
                // try the next candidate.
                Ok(false) => continue,
                Err(_) => return false,
            }

            // Phase 3: drop the slot, but only if it still holds the address we
            // authorised. The generation bump has already invalidated it either
            // way, so a `false` here is safe -- it means the slot was refilled
            // and a newer value now owns it.
            let r = matches!(tier.remove_if_address(address), Ok(true));
            return r;
        }
        let _ = key;
        false
    }

    pub async fn invalidate(&self, key: &K, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;
        let request = Self::request_for(CacheOperation::Invalidate, key, ctx);
        let decision = self.resolve(&request, ctx);
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;
        self.cachelito.invalidate(key_ref.0)?;
        Ok(())
    }

    pub async fn remove(&self, key: &K, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;
        let request = Self::request_for(CacheOperation::Remove, key, ctx);
        let decision = self.resolve(&request, ctx);
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;

        for (idx, tier) in self.tiers.iter().enumerate() {
            // Authority rule (tripwired by `invalidate_skips_l6_authority`):
            // removing the authority row would delete the record of truth. Only
            // cache rungs are cleared; authority writes invalidate downward.
            if idx == TierId::L6.as_usize() {
                continue;
            }
            // Best-effort: a tier may legitimately not hold the key.
            let _ = tier.remove(&key_ref).await;
        }
        self.cachelito.release(key_ref.0)?;
        Ok(())
    }

    pub async fn exists(&self, key: &K, ctx: &CacheContext) -> Result<bool, CacheError> {
        Self::check_auth(ctx)?;
        let request = Self::request_for(CacheOperation::Exists, key, ctx);
        let decision = self.resolve(&request, ctx);
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;
        // `peek`, never `acquire`: a read-only probe that claims the entry
        // leaves the key in `InFlight` with an owner nobody will ever satisfy,
        // so every later operation on that key waits out the full timeout.
        let snapshot = self.cachelito.peek(key_ref.0)?;

        if snapshot.state != EntryState::Ready {
            return Ok(false);
        }
        let Some(tier) = self.bound_tier(&snapshot.tier) else {
            return Ok(false);
        };
        self.bounded(tier.contains(&key_ref)).await
    }

    /// Stale-while-revalidate, reporting whether the served value is a fallback.
    ///
    /// This is the primitive; [`CacheManager::refresh`] is the lossy wrapper over
    /// it, kept because callers that never revalidate have no use for the
    /// distinction. Callers that do should prefer this, because the wrapper
    /// discards exactly the information that makes stale-while-revalidate safe to
    /// reason about.
    pub async fn refresh_detailed<F, Fut>(
        &self,
        key: &K,
        ctx: &CacheContext,
        fetch: F,
    ) -> Result<Lookup<V>, CacheError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<V, CacheError>>,
    {
        Self::check_auth(ctx)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;
        let request = Self::request_for(CacheOperation::Refresh, key, ctx);
        let mut decision = self.resolve(&request, ctx);
        Self::authorize(&decision)?;

        // `refresh` fails *closed* on the fetch itself, then applies its own
        // documented stale-while-revalidate fallback below.
        //
        // Leaving the policy's fail-open mode in place let `become_population_owner`
        // scan other rungs and return their value as a successful population, which
        // this method then had no way to distinguish from a value the fetch just
        // produced. That is `cia.integrity.silent_stale_data_acceptance = false`
        // failing at the only place it could: not by serving stale bytes, but by
        // serving them with a success status that says they were fresh.
        decision.fail_mode = FailMode::Closed;

        // Peek first: `refresh` reads the current value before deciding whether
        // it needs to revalidate, and `acquire` would claim ownership as a side
        // effect of that read.
        let before = self.cachelito.peek(key_ref.0)?;
        let current = if before.state.is_readable() {
            // A tier error means "I cannot tell you the current value", which is
            // not the same as "there is none". Swallowing it into `None` would
            // report a present value as absent.
            match self.bound_tier(&before.tier) {
                Some(tier) => self.bounded(tier.get(&key_ref)).await.unwrap_or_default(),
                None => None,
            }
        } else {
            None
        };

        // Revalidating a `Ready` entry needs it to be claimable, and `Ready` is
        // not: `acquire` on a committed entry deliberately declines. So mark it
        // stale first — which is exactly what TTL expiry does — and then claim.
        //
        // Without this, `refresh` on a fresh entry returned the current value
        // forever: the claim always lost to `Ready`, and the `!owner` branch
        // served stale. `refresh` is documented as revalidating, so that is a
        // silent no-op rather than the stale-while-revalidate it claims to be.
        if before.state == EntryState::Ready && !before.expired {
            let _ = self.cachelito.set_state(key_ref.0, EntryState::Stale);
        }

        // Only claim if nothing else is already doing the work. Concurrent
        // refreshes used to each become "the owner" and all publish, which is
        // the stampede this crate is supposed to prevent.
        let snapshot = self.cachelito.acquire(key_ref.0, decision.tier)?;
        if !snapshot.population_owner {
            // Someone is already revalidating. Serve what we have; a refresh is
            // explicitly allowed to be served from a stale value -- and now says
            // so, rather than returning it indistinguishably from a fresh one.
            return Ok(Lookup {
                value: current,
                freshness: Freshness::Stale,
            });
        }

        match self
            .become_population_owner(key, key_ref, snapshot.generation, ctx, &decision, fetch)
            .await
        {
            Ok(new_value) => Ok(Lookup {
                value: Some(new_value),
                freshness: Freshness::Fresh,
            }),
            // Stale-while-revalidate: serve the old value when revalidation
            // fails. This is the documented contract of `refresh`, so it is not
            // an error swallow — but it must not hide the *first* failure, when
            // there is nothing stale to serve.
            Err(e) if current.is_some() => {
                self.record_telemetry_refresh_failure(&key_ref, &snapshot, e);
                Ok(Lookup {
                    value: current,
                    freshness: Freshness::Stale,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Stale-while-revalidate, discarding whether the served value was a fallback.
    ///
    /// Prefer [`CacheManager::refresh_detailed`] where the distinction matters: this
    /// wrapper cannot tell a caller that `Some(v)` arrived as stale, which is the
    /// whole point of `cia.integrity.silent_stale_data_acceptance = false`.
    pub async fn refresh<F, Fut>(
        &self,
        key: &K,
        ctx: &CacheContext,
        fetch: F,
    ) -> Result<Option<V>, CacheError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<V, CacheError>>,
    {
        self.refresh_detailed(key, ctx, fetch)
            .await
            .map(|l| l.value)
    }

    pub async fn promote(&self, key: &K, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;
        let request = Self::request_for(CacheOperation::Promote, key, ctx);
        let decision = self.resolve(&request, ctx);
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;
        // Peek, not acquire: promotion must not take population ownership.
        let snapshot = self.cachelito.peek(key_ref.0)?;
        if snapshot.state != EntryState::Ready {
            return Err(CacheError::Miss);
        }

        // An entry sitting on the authority rung is not promotable, and the
        // rung below the ladder floor is not either. Both bounds are expressed
        // against the ladder constant so authority cannot be promoted *into* a
        // cache rung by an off-by-one.
        if !snapshot.tier.is_cache_rung() || snapshot.tier.as_usize() == 0 {
            return Ok(());
        }
        let new_tier_id = TierId::from_usize(snapshot.tier.as_usize() - 1)
            .ok_or(CacheError::ConfigurationError)?;
        self.move_entry(key, &key_ref, &snapshot, new_tier_id, ctx)
            .await
    }

    pub async fn demote(&self, key: &K, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;
        let request = Self::request_for(CacheOperation::Demote, key, ctx);
        let decision = self.resolve(&request, ctx);
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;
        let snapshot = self.cachelito.peek(key_ref.0)?;
        if snapshot.state != EntryState::Ready {
            return Err(CacheError::Miss);
        }

        // Bounded by the ladder constant rather than by a literal tier, so the
        // bound lives in exactly one place and the contract's `last_cache_rung`
        // has something to be checked against. An entry already on the authority
        // has nowhere to move: this is a no-op rather than an error.
        if !snapshot.tier.is_cache_rung() || snapshot.tier >= crate::policy::LAST_CACHE_TIER {
            return Ok(());
        }
        let new_tier_id = TierId::from_usize(snapshot.tier.as_usize() + 1)
            .ok_or(CacheError::ConfigurationError)?;
        self.move_entry(key, &key_ref, &snapshot, new_tier_id, ctx)
            .await
    }

    /// Moves an entry between tiers, so it awaits the source read and the
    /// destination write. It is async for that reason alone: the control-plane
    /// work around those two awaits (`cachelito.set_tier`, `record_outcome`)
    /// holds no guard across them, per the no-guard-across-await rule.
    async fn move_entry(
        &self,
        key: &K,
        key_ref: &KeyRef<'_>,
        snapshot: &ControlSnapshot,
        new_tier_id: TierId,
        ctx: &CacheContext,
    ) -> Result<(), CacheError> {
        // Phase 1: the intent names both ends, so recovery knows this is a move
        // of an already-committed value and must complete it forward rather than
        // aborting it into data loss.
        let token = self
            .cachelito
            .prepare(key_ref.0, None, new_tier_id, IntentKind::Move)?;

        // As in `set`: the guard owns the intent from here, so every early return
        // below resolves it. Two of those returns were previously leaks rather
        // than merely cancellation-unsafe — the unbound-tier check and the key
        // re-encode both returned while the entry was still `Prepared`.
        let mut intent = IntentGuard::new(&self.cachelito, token);

        let (Some(from_tier), Some(to_tier)) = (
            self.bound_tier(&snapshot.tier),
            self.bound_tier(&new_tier_id),
        ) else {
            return Err(CacheError::TierUnavailable);
        };

        // Read the source before touching the destination, so a missing source
        // aborts without having written anything.
        let value = match self.bounded(from_tier.get(key_ref)).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                intent.abort(CacheError::Miss);
                return Err(CacheError::Miss);
            }
            Err(e) => {
                intent.abort(e);
                return Err(e);
            }
        };

        let mut buf2 = [0u8; MAX_KEY_SIZE];
        let key_ref2 = match Self::encode_key(key, ctx, &mut buf2) {
            Ok(k) => k,
            Err(e) => {
                intent.abort(e);
                return Err(e);
            }
        };
        let set_result = self
            .observed_write(
                Operation::Promote,
                &key_ref2,
                new_tier_id,
                to_tier.set(&key_ref2, value, ctx.ttl()),
            )
            .await;
        self.record_outcome(new_tier_id, set_result);
        if let Err(e) = set_result {
            intent.abort(e);
            return Err(e);
        }

        // The move is a move, not a copy. Leaving the payload in the source rung
        // meant the value existed in two places with one control-plane record,
        // and a later write to the source could resurrect a superseded value
        // under a fresh generation.
        let remove_result = from_tier.remove(key_ref).await;
        self.record_outcome(snapshot.tier, remove_result);

        match self.cachelito.commit(intent.token(), ctx.ttl()) {
            Ok(()) => {
                intent.disarm();
                Ok(())
            }
            Err(e) => {
                // Uncommitted: the destination copy is residue. Remove it so the
                // entry is only on the rung the control plane names.
                let _ = to_tier.remove(&key_ref2).await;
                intent.abort(e);
                Err(e)
            }
        }
    }

    /// The best bound rung at or below `wanted`.
    ///
    /// # Routing, not substitution
    ///
    /// Policy names a rung it would *like*; this finds the nearest one that is
    /// actually bound at or below it. That is a different operation from the
    /// substitution this crate used to do, and the difference matters:
    ///
    /// * Substitution answered a caller's *request* for `L6` with L0's tier
    ///   object, so the caller could not tell it had been given something else.
    ///   The contract forbids that.
    /// * This resolves the policy's *choice*, before any caller has named a rung,
    ///   and the result is reported through `capabilities()` and telemetry. A
    ///   manager bound only to L0 answering a policy that asked for L1 is a
    ///   routing decision, not a lie.
    ///
    /// Without it, a one-rung manager is unusable: `DefaultPolicy` selects L1 for
    /// a write, nothing is bound there, and every write fails as
    /// `TierUnavailable`. The old substitution hid that by quietly using L0.
    ///
    /// The walk is bounded at L0 and refuses to leave the ladder, so it can never
    /// reach the authority rung.
    #[must_use]
    pub fn nearest_bound_rung(&self, wanted: TierId) -> Option<TierId> {
        if !wanted.is_cache_rung() {
            return None;
        }
        let mut idx = wanted.as_usize();
        loop {
            let candidate = TierId::from_index(u8::try_from(idx).ok()?)?;
            if self.has_tier(&candidate) {
                return Some(candidate);
            }
            if idx == 0 {
                return None;
            }
            idx -= 1;
        }
    }

    /// The rung bound at `tier_id`, or `None` if nothing is bound there.
    ///
    /// # Why this returns `Option`
    ///
    /// It used to substitute: an out-of-range id resolved to `tiers[0]`, and an
    /// empty tier list indexed `tiers[0]` and panicked. Both are wrong, and the
    /// contract forbids them explicitly.
    ///
    /// Substitution is worse than the error it avoids because it is invisible: a
    /// caller asking for an unbound L6 received L0's tier object, and a
    /// subsequent read returned L0's *data* labelled as a lower rung's. A
    /// fail-open fallback scan bounded only by "is it not this one" would walk
    /// straight into that and surface a value from the wrong rung. Returning
    /// `None` makes the gap checkable instead of merely documented.
    ///
    /// This is a breaking change in a major release on purpose: the previous
    /// signature could not express "there is nothing there".
    #[must_use]
    pub fn bound_tier(&self, tier_id: &TierId) -> Option<Arc<dyn CacheTier<V>>> {
        self.tiers.get(tier_id.as_usize()).cloned()
    }

    /// What every configured tier is actually bound to, keyed by tier id.
    ///
    /// This is the answer to the question that could previously only be
    /// discovered by issuing an operation and catching `TierUnavailable`. A
    /// caller can now fail fast at construction — refuse to start if the
    /// distributed tier is really an in-memory fallback, say — instead of
    /// discovering it on a live read path in production.
    ///
    /// A rung that is not bound at all reports
    /// [`OperationalState::Unbound`](crate::OperationalState::Unbound), which is
    /// deliberately distinct from
    /// [`OperationalState::Unavailable`](crate::OperationalState::Unavailable):
    /// a bound rung whose backend is down is
    /// a runtime condition, an unbound one is a build or configuration error,
    /// and the two want opposite responses.
    ///
    /// The returned capability comes from the tier instance itself, so a rung
    /// cannot report a backend or a durability class it was not constructed
    /// with. See [`crate::capability`] for why this is a struct of orthogonal
    /// axes rather than one enum.
    #[must_use]
    pub fn capabilities(&self) -> std::collections::BTreeMap<TierId, TierCapability> {
        TierId::ALL
            .iter()
            .map(|id| {
                let mut cap = if self.has_tier(id) {
                    self.bound_tier(id)
                        .map_or_else(TierCapability::unbound, |t| t.capability())
                } else {
                    TierCapability::unbound()
                };
                // Authority is a *configured role*, so it is stamped here rather
                // than read from the tier. A tier implementation cannot know
                // whether it is the system of record — that depends on how the
                // application wired the ladder — and asking it to guess is how
                // authority ends up implied by a tier number.
                //
                // The flag is applied to the authority rung whether or not it is
                // bound, because "the authority is not configured" and "the
                // authority rung has no implementation" are different facts and
                // the capability surface exists to keep them apart.
                if *id == self.authority_tier {
                    cap.flags |= crate::capability::CapabilityFlags::AUTHORITATIVE;
                }
                (*id, cap)
            })
            .collect()
    }

    /// Whether `tier_id` is actually bound in this manager.
    ///
    /// Equivalent to `bound_tier(id).is_some()`, kept as a named predicate
    /// because "is it there?" and "give me it" are different questions and
    /// conflating them is what the substitution shim did.
    #[must_use]
    pub fn has_tier(&self, tier_id: &TierId) -> bool {
        tier_id.as_usize() < self.tiers.len()
    }

    /// The rung bound at `tier_id`, or `None` if nothing is bound there.
    ///
    /// An alias for [`Self::bound_tier`], kept because `tier(&id)` reads better
    /// at a call site that has already checked. Returns `Option` rather than
    /// substituting another rung: the previous signature returned `tiers[0]` for
    /// an unbound id and *panicked* on an empty ladder, and a property test
    /// found that panic by generating a manager with no rungs at all. An error
    /// is the correct answer to "there is nothing there".
    #[must_use]
    pub fn tier(&self, tier_id: &TierId) -> Option<Arc<dyn CacheTier<V>>> {
        self.bound_tier(tier_id)
    }

    /// The backend bound at `tier_id`, or `Unavailable` if nothing is bound.
    ///
    /// For telemetry and capability reporting, where "what is this rung" must be
    /// answerable even when the rung is missing.
    fn backend_of(&self, tier_id: TierId) -> crate::tier::tier_trait::BackendKind {
        self.bound_tier(&tier_id)
            .map_or(crate::tier::tier_trait::BackendKind::Unavailable, |t| {
                t.backend()
            })
    }

    /// Wait for another caller's population to finish.
    ///
    /// The waiter registers *before* deciding whether to wait. `Notified` does
    /// not register until it is first polled, so the previous shape raced: a
    /// waiter that observed `InFlight`, released the shard lock, and was
    /// preempted before `select!` polled its future would miss the owner's
    /// notify entirely and sit out the full timeout reporting a spurious
    /// `Timeout` for a population that had already succeeded.
    async fn wait_for_population(
        &self,
        key_ref: &KeyRef<'_>,
        snapshot: &ControlSnapshot,
    ) -> Result<(), CacheError> {
        let notified = snapshot.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // Having registered, re-observe: the population may have completed in the
        // gap, in which case there is nothing to wait for and returning
        // immediately is both correct and faster than waking on a stored permit.
        match self.cachelito.peek(key_ref.0) {
            Ok(now) if now.state != EntryState::InFlight && now.state != EntryState::Prepared => {
                return Ok(());
            }
            Ok(_) => {}
            // A poisoned shard or a missing entry: fall through and wait, which
            // is the pre-existing behaviour and cannot make things worse.
            Err(_) => {}
        }

        tokio::select! {
            _ = tokio::time::sleep(self.wait_timeout) => Err(CacheError::Timeout),
            _ = &mut notified => Ok(()),
        }
    }

    /// Join another caller's population and return its result.
    ///
    /// Everything is re-read from the control plane *after* the wait. The
    /// previous version read `snapshot.tier` and `snapshot.last_error` from the
    /// waiter's own pre-wait snapshot, which is stale in both cases: the tier is
    /// the one the entry had *before* the owner published, and `last_error` had
    /// been cleared by the owner's `acquire`, so a waiter could never actually
    /// receive the owner's failure.
    async fn wait_and_get(
        &self,
        key: &K,
        ctx: &CacheContext,
        snapshot: &ControlSnapshot,
    ) -> Result<V, CacheError> {
        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;
        self.wait_for_population(&key_ref, snapshot).await?;

        let after = self.cachelito.peek(key_ref.0)?;

        // The owner's terminal error, if it recorded one. This is the path the
        // contract calls `waiters_receive_population_error`, and reading it
        // post-wait is what makes it reachable at all.
        if let Some(err) = after.last_error {
            return Err(err);
        }

        // The owner may have committed to a different rung than the one this
        // waiter observed before waiting.
        if !after.state.is_readable() {
            return Err(CacheError::Miss);
        }

        let Some(tier) = self.bound_tier(&after.tier) else {
            return Err(CacheError::TierUnavailable);
        };
        match self
            .observed_read(
                Operation::GetOrFetch,
                &key_ref,
                after.tier,
                tier.get(&key_ref),
            )
            .await?
        {
            Some(v) => Ok(v),
            // The control plane says Ready but the rung has nothing. Report the
            // disagreement rather than looping: a retry loop here would be an
            // unbounded repopulation attempt.
            None => Err(CacheError::Miss),
        }
    }

    async fn become_population_owner<F, Fut>(
        &self,
        key: &K,
        key_ref: KeyRef<'_>,
        expected_generation: Generation,
        ctx: &CacheContext,
        decision: &crate::policy::PolicyDecision,
        fetch: F,
    ) -> Result<V, CacheError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<V, CacheError>>,
    {
        let mut backoff = RETRY_BASE_BACKOFF;
        let mut attempt = 0_u32;
        let generation = expected_generation;

        // Armed for the whole retry sequence and disarmed only on a terminal
        // outcome, so cancellation or a panic at any point still releases the
        // claim.
        let mut guard = PopulationGuard::new(&self.cachelito, key_ref.0);

        // Bounded retry loop (Rule 2): at most MAX_POPULATE_ATTEMPTS. The entry
        // is owned once (acquired in the caller); we retry the fetch in place
        // and only release the entry to Failed on terminal failure, so waiters
        // observe a single population attempt for the whole retry sequence.
        loop {
            attempt += 1;
            let outcome = self
                .populate_once(key, &key_ref, generation, ctx, &fetch, decision)
                .await;

            match outcome {
                Ok(value) => {
                    guard.disarm();
                    return Ok(value);
                }
                Err(e) if attempt < MAX_POPULATE_ATTEMPTS && Self::is_retryable(e) => {
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2);
                }
                Err(e) => {
                    // A population that fails is exactly the operation you most
                    // want in the log. Recording only the tier accesses meant a
                    // fetch failure produced *no* record at all, so the failure
                    // rate the contract asks for was unmeasurable — and the
                    // fallback scan records only when it finds something, so a
                    // total miss contributed nothing either.
                    let started = std::time::Instant::now();
                    self.emit(
                        OperationRecord::begin(Operation::GetOrFetch, &key_ref)
                            .with_destination(decision.tier)
                            .with_backend(self.backend_of(decision.tier))
                            .with_failure(e)
                            .finish(started, crate::telemetry::Outcome::from_error(e)),
                    );
                    // Ownership MUST be released on every terminal path,
                    // including the one that used to return early. A population
                    // that bailed on `StaleGeneration` left the entry `InFlight`
                    // with `population_owner = true` and no notify pending, so
                    // every later operation on that key joined as a waiter,
                    // waited out the full timeout, and failed — a key stayed
                    // unusable until someone invalidated it.
                    // The claim is released here explicitly, so the guard can be
                    // disarmed rather than doing it on the way out.
                    let _ = self.cachelito.fail_with_error(key_ref.0, error_kind(e));
                    guard.disarm();
                    if matches!(e, CacheError::StaleGeneration) {
                        return Err(CacheError::StaleGeneration);
                    }
                    if decision.fail_mode == FailMode::Open {
                        return self.try_fallback_tier(key, ctx, e).await;
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn populate_once<F, Fut>(
        &self,
        key: &K,
        key_ref: &KeyRef<'_>,
        expected_generation: Generation,
        ctx: &CacheContext,
        fetch: &F,
        decision: &crate::policy::PolicyDecision,
    ) -> Result<V, CacheError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<V, CacheError>>,
    {
        let result = tokio::time::timeout(self.wait_timeout, fetch()).await;
        let value = match result {
            Ok(Ok(value)) => value,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(CacheError::Timeout),
        };

        let Some(tier_id) = self.nearest_bound_rung(decision.tier) else {
            return Err(CacheError::TierUnavailable);
        };
        let Some(tier) = self.bound_tier(&tier_id) else {
            return Err(CacheError::TierUnavailable);
        };
        let mut buf2 = [0u8; MAX_KEY_SIZE];
        let key_ref2 = Self::encode_key(key, ctx, &mut buf2)?;
        let set_result = self
            .observed_write(
                Operation::GetOrFetch,
                &key_ref2,
                tier_id,
                tier.set(&key_ref2, value.clone(), ctx.ttl()),
            )
            .await;
        self.record_outcome(tier_id, set_result);
        set_result?;

        self.cachelito
            .publish(key_ref.0, expected_generation, tier_id, ctx.ttl())?;
        Ok(value)
    }

    fn is_retryable(err: CacheError) -> bool {
        matches!(
            err,
            CacheError::TierUnavailable | CacheError::Timeout | CacheError::PopulationFailed
        )
    }
    /// Fail-open: look for an already-resident copy on another cache rung.
    ///
    /// Three properties this must have, each of which it previously lacked:
    ///
    /// * **Bounded by the ladder.** Iterating `TierRegistry::all()` walked the
    ///   authority rung too, so a fallback could surface authority data under a
    ///   fallback's name — and on a six-rung manager `tier_for(L6)` substituted
    ///   L0, so the scan could return L0's bytes as though a lower rung held
    ///   them. That is authority inversion and silent backend substitution, both
    ///   forbidden by the contract.
    /// * **Never substitutes.** Unbound rungs are skipped rather than resolved
    ///   through `tier_for`'s compatibility shim.
    /// * **Preserves the cause.** A bare `PopulationFailed` discarded the error
    ///   that caused the fallback, so a timeout, a capacity failure and a fetch
    ///   error were indistinguishable at the call site.
    async fn try_fallback_tier(
        &self,
        key: &K,
        ctx: &CacheContext,
        cause: CacheError,
    ) -> Result<V, CacheError> {
        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, ctx, &mut buf)?;

        for tier_id in crate::policy::cache_ladder() {
            if !self.has_tier(&tier_id) {
                continue;
            }
            if self.tier_registry.is_circuit_open(tier_id) {
                continue;
            }
            let Some(tier) = self.bound_tier(&tier_id) else {
                continue;
            };
            match self.bounded(tier.get(&key_ref)).await {
                Ok(Some(v)) => {
                    self.record_fallback(&key_ref, tier_id, cause);
                    return Ok(v);
                }
                Ok(None) => continue,
                Err(_) => continue,
            }
        }
        Err(cause)
    }
}
