use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use crate::control::cachelito::{Cachelito, ControlSnapshot};
use crate::entry::{EntryState, Generation};
use crate::error::CacheError;
use crate::identity::CacheContext;
use crate::key::Key;
use crate::key::KeyRef;
use crate::policy::{CacheOperation, CachePolicy, CacheRequest, CacheState, FailMode};
use crate::pool::MemoryPool;
use crate::tier::tier_trait::CacheTier;
use crate::tier::{TierId, TierRegistry};

const MAX_KEY_SIZE: usize = 256;
/// Maximum populate attempts per `stampede.toml` ([retry] `max_attempts`).
const MAX_POPULATE_ATTEMPTS: u32 = 3;
/// Base backoff for populate retries; doubled each attempt (exponential).
const RETRY_BASE_BACKOFF: Duration = Duration::from_millis(50);

/// Identity helper retained for clarity at the call site; `CacheError` is
/// `Copy`, so this simply returns the error for the control-plane marker.
fn error_kind(err: CacheError) -> CacheError {
    err
}

pub struct CacheManager<K, V, P> {
    policy: P,
    cachelito: Cachelito,
    tier_registry: TierRegistry,
    tiers: Vec<Arc<dyn CacheTier<V>>>,
    _pool: MemoryPool<V>,
    wait_timeout: Duration,
    _key: PhantomData<K>,
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
            _key: PhantomData,
        }
    }

    pub fn cachelito(&self) -> &Cachelito {
        &self.cachelito
    }

    fn encode_key<'a>(key: &K, buf: &'a mut [u8; MAX_KEY_SIZE]) -> Result<KeyRef<'a>, CacheError> {
        let len = key.encode(buf)?;
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
    fn resolve(
        &self,
        op: CacheOperation,
        key: &K,
        ctx: &CacheContext,
    ) -> Result<crate::policy::PolicyDecision, CacheError> {
        let request = Self::request_for(op, key, ctx);
        let state = CacheState::new();
        Ok(self.policy.select(&request, &state, ctx.identity()))
    }

    /// Resolve the policy decision against a real snapshot from the control
    /// plane (used on the populate/hit paths).
    fn resolve_with_snapshot(
        &self,
        op: CacheOperation,
        key: &K,
        ctx: &CacheContext,
        snapshot: &ControlSnapshot,
    ) -> Result<crate::policy::PolicyDecision, CacheError> {
        let request = Self::request_for(op, key, ctx);
        let health = self.tier_registry.tier_health(snapshot.tier);
        let state = CacheState::from_snapshot(snapshot, health);
        Ok(self.policy.select(&request, &state, ctx.identity()))
    }

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
        Self::check_auth(ctx)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;

        let decision = self.resolve(CacheOperation::Get, key, ctx)?;
        Self::authorize(&decision)?;

        let snapshot = self.cachelito.acquire(key_ref.0, TierId::L0)?;

        match snapshot.state {
            EntryState::Ready if !snapshot.expired => {
                let tier = self.tier_for(&snapshot.tier);
                let result = tier.get(&key_ref);
                self.record_read(snapshot.tier, &result);
                result
            }
            EntryState::Ready => {
                // TTL elapsed: mark Stale and report a miss (lazy expiry).
                let _ = self.cachelito.set_state(key_ref.0, EntryState::Stale);
                Err(CacheError::Miss)
            }
            EntryState::InFlight if !snapshot.population_owner => {
                self.wait_for_population(&snapshot).await?;
                let mut buf2 = [0u8; MAX_KEY_SIZE];
                let key_ref2 = Self::encode_key(key, &mut buf2)?;
                let tier = self.tier_for(&snapshot.tier);
                let result = tier.get(&key_ref2);
                self.record_read(snapshot.tier, &result);
                result
            }
            _ => Err(CacheError::Miss),
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
        let key_ref = Self::encode_key(key, &mut buf)?;

        let decision = self.resolve(CacheOperation::Get, key, ctx)?;
        Self::authorize(&decision)?;

        let snapshot = self.cachelito.acquire(key_ref.0, TierId::L0)?;

        if snapshot.state == EntryState::Ready && !snapshot.expired {
            let tier = self.tier_for(&snapshot.tier);
            if let Some(v) = tier.get(&key_ref)? {
                return Ok(v);
            }
        }

        // The acquire snapshot that drives population. For a TTL-expired Ready
        // entry we first transition it to Stale, then re-acquire to claim
        // ownership (Stale is claimable), so publish uses an owned generation.
        let pop_snapshot = if snapshot.state == EntryState::Ready && snapshot.expired {
            let _ = self.cachelito.set_state(key_ref.0, EntryState::Stale);
            self.cachelito.acquire(key_ref.0, decision.tier)?
        } else {
            snapshot
        };

        if pop_snapshot.state == EntryState::InFlight && !pop_snapshot.population_owner {
            return self.wait_and_get(key, &pop_snapshot).await;
        }

        self.become_population_owner(key, key_ref, pop_snapshot.generation, ctx, &decision, fetch)
            .await
    }

    pub async fn set(&self, key: &K, value: V, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;

        let snapshot = self.cachelito.acquire(key_ref.0, TierId::L0)?;
        let decision = self.resolve_with_snapshot(CacheOperation::Set, key, ctx, &snapshot)?;
        Self::authorize(&decision)?;

        let tier = self.tier_for(&decision.tier);
        let set_result = tier.set(&key_ref, value, ctx.ttl());
        self.record_outcome(decision.tier, set_result);
        set_result?;

        let generation = Generation::new(snapshot.generation.0 + 1);
        self.cachelito.set_generation(key_ref.0, generation)?;
        self.cachelito
            .publish(key_ref.0, generation, decision.tier, ctx.ttl())?;

        Ok(())
    }

    pub async fn invalidate(&self, key: &K, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;
        let decision = self.resolve(CacheOperation::Invalidate, key, ctx)?;
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;
        self.cachelito.invalidate(key_ref.0)?;
        Ok(())
    }

    pub async fn remove(&self, key: &K, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;
        let decision = self.resolve(CacheOperation::Remove, key, ctx)?;
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;

        for tier in &self.tiers {
            let _ = tier.remove(&key_ref);
        }
        self.cachelito.release(key_ref.0)?;
        Ok(())
    }

    pub async fn exists(&self, key: &K, ctx: &CacheContext) -> Result<bool, CacheError> {
        Self::check_auth(ctx)?;
        let decision = self.resolve(CacheOperation::Exists, key, ctx)?;
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;
        let snapshot = self.cachelito.acquire(key_ref.0, TierId::L0)?;

        if snapshot.state != EntryState::Ready {
            return Ok(false);
        }
        let tier = self.tier_for(&snapshot.tier);
        tier.contains(&key_ref)
    }

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
        Self::check_auth(ctx)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;
        let decision = self.resolve(CacheOperation::Refresh, key, ctx)?;
        Self::authorize(&decision)?;

        let snapshot = self.cachelito.acquire(key_ref.0, TierId::L0)?;
        let current_tier = snapshot.tier;
        let current = {
            let tier = self.tier_for(&current_tier);
            tier.get(&key_ref).unwrap_or(None)
        };

        match self
            .become_population_owner(key, key_ref, snapshot.generation, ctx, &decision, fetch)
            .await
        {
            Ok(new_value) => Ok(Some(new_value)),
            // Stale-while-revalidate: serve the old value when refresh fails.
            Err(_) => Ok(current),
        }
    }

    pub async fn promote(&self, key: &K, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;
        let decision = self.resolve(CacheOperation::Promote, key, ctx)?;
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;
        let snapshot = self.cachelito.acquire(key_ref.0, TierId::L0)?;
        if snapshot.state != EntryState::Ready {
            return Err(CacheError::Miss);
        }

        let current_idx = snapshot.tier.as_usize();
        if current_idx == 0 {
            return Ok(());
        }
        let new_tier_id =
            TierId::from_usize(current_idx - 1).ok_or(CacheError::ConfigurationError)?;
        self.move_entry(key, &key_ref, &snapshot, new_tier_id, ctx)
    }

    pub async fn demote(&self, key: &K, ctx: &CacheContext) -> Result<(), CacheError> {
        Self::check_auth(ctx)?;
        let decision = self.resolve(CacheOperation::Demote, key, ctx)?;
        Self::authorize(&decision)?;

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;
        let snapshot = self.cachelito.acquire(key_ref.0, TierId::L0)?;
        if snapshot.state != EntryState::Ready {
            return Err(CacheError::Miss);
        }

        let current_idx = snapshot.tier.as_usize();
        if current_idx == TierId::L5.as_usize() {
            return Ok(());
        }
        let new_tier_id =
            TierId::from_usize(current_idx + 1).ok_or(CacheError::ConfigurationError)?;
        self.move_entry(key, &key_ref, &snapshot, new_tier_id, ctx)
    }

    fn move_entry(
        &self,
        key: &K,
        key_ref: &KeyRef<'_>,
        snapshot: &ControlSnapshot,
        new_tier_id: TierId,
        ctx: &CacheContext,
    ) -> Result<(), CacheError> {
        let from_tier = self.tier_for(&snapshot.tier);
        let value = from_tier.get(key_ref)?.ok_or(CacheError::Miss)?;

        let to_tier = self.tier_for(&new_tier_id);
        let mut buf2 = [0u8; MAX_KEY_SIZE];
        let key_ref2 = Self::encode_key(key, &mut buf2)?;
        let set_result = to_tier.set(&key_ref2, value, ctx.ttl());
        self.record_outcome(new_tier_id, set_result);
        set_result?;

        self.cachelito.set_tier(key_ref.0, new_tier_id)?;
        Ok(())
    }

    pub fn tier_for(&self, tier_id: &TierId) -> Arc<dyn CacheTier<V>> {
        let idx = tier_id.as_usize();
        if idx >= self.tiers.len() {
            // Defensive: configuration guarantees 6 tiers; fall back to L0.
            return self.tiers[0].clone();
        }
        self.tiers[idx].clone()
    }

    pub fn tier(&self, tier_id: &TierId) -> Arc<dyn CacheTier<V>> {
        self.tier_for(tier_id)
    }

    async fn wait_for_population(&self, snapshot: &ControlSnapshot) -> Result<(), CacheError> {
        tokio::select! {
            _ = tokio::time::sleep(self.wait_timeout) => {
                Err(CacheError::Timeout)
            }
            _ = snapshot.notify.notified() => {
                Ok(())
            }
        }
    }

    async fn wait_and_get(&self, key: &K, snapshot: &ControlSnapshot) -> Result<V, CacheError> {
        self.wait_for_population(snapshot).await?;

        // The owner has finished. If it failed, surface the recorded terminal
        // error to this waiter (stampede.toml `waiters_receive_population_error`).
        if let Some(err) = snapshot.last_error {
            return Err(err);
        }

        let mut buf = [0u8; MAX_KEY_SIZE];
        let key_ref = Self::encode_key(key, &mut buf)?;

        let tier = self.tier_for(&snapshot.tier);
        match tier.get(&key_ref)? {
            Some(v) => Ok(v),
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
                Ok(value) => return Ok(value),
                Err(e) if attempt < MAX_POPULATE_ATTEMPTS && Self::is_retryable(e) => {
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2);
                }
                Err(e) => {
                    if matches!(e, CacheError::StaleGeneration) {
                        return Err(CacheError::StaleGeneration);
                    }
                    let _ = self.cachelito.fail_with_error(key_ref.0, error_kind(e));
                    if decision.fail_mode == FailMode::Open {
                        return self.try_fallback_tier(key).await;
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

        let tier_id = decision.tier;
        let tier = self.tier_for(&tier_id);
        let mut buf2 = [0u8; MAX_KEY_SIZE];
        let key_ref2 = Self::encode_key(key, &mut buf2)?;
        let set_result = tier.set(&key_ref2, value.clone(), ctx.ttl());
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
    async fn try_fallback_tier(&self, key: &K) -> Result<V, CacheError> {
        for tier_id in self.tier_registry.all() {
            if self.tier_registry.is_circuit_open(*tier_id) {
                continue;
            }
            let tier = self.tier_for(tier_id);
            let mut buf = [0u8; MAX_KEY_SIZE];
            let key_ref = Self::encode_key(key, &mut buf)?;
            match tier.get(&key_ref) {
                Ok(Some(v)) => return Ok(v),
                Ok(None) => continue,
                Err(_) => continue,
            }
        }
        Err(CacheError::PopulationFailed)
    }
}
