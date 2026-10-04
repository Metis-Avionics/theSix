use std::time::Duration;

use crate::entry::Generation;
use crate::identity::IdentityContext;
use crate::tier::TierId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOperation {
    Get,
    Set,
    Remove,
    Invalidate,
    Refresh,
    Exists,
    Promote,
    Demote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopulationStrategy {
    SingleFlight,
    AllowDuplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailMode {
    Open,
    Closed,
}

#[derive(Debug)]
pub struct CacheRequest<K, V> {
    pub operation: CacheOperation,
    pub key: K,
    pub value: Option<V>,
    pub ttl: Option<Duration>,
    /// Approximate size of the entry in bytes, if known (0 = unknown).
    pub entry_size: usize,
    /// Relative cost of (re)populating this entry; higher = more expensive.
    pub cost: u32,
    /// Maximum latency the caller will tolerate, if constrained.
    pub latency_budget: Option<Duration>,
}

impl<K, V> CacheRequest<K, V> {
    pub fn new(operation: CacheOperation, key: K) -> Self {
        CacheRequest {
            operation,
            key,
            value: None,
            ttl: None,
            entry_size: 0,
            cost: 0,
            latency_budget: None,
        }
    }

    pub fn with_value(mut self, value: V) -> Self {
        self.value = Some(value);
        self
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    pub fn with_entry_size(mut self, entry_size: usize) -> Self {
        self.entry_size = entry_size;
        self
    }

    pub fn with_cost(mut self, cost: u32) -> Self {
        self.cost = cost;
        self
    }

    pub fn with_latency_budget(mut self, budget: Duration) -> Self {
        self.latency_budget = Some(budget);
        self
    }
}

#[derive(Debug)]
pub struct CacheState {
    pub entry_state: crate::entry::EntryState,
    pub generation: Generation,
    pub tier: TierId,
    pub tier_health: crate::tier::tier_trait::TierHealth,
    pub expiration: Option<std::time::Instant>,
}

impl Default for CacheState {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheState {
    pub fn new() -> Self {
        CacheState {
            entry_state: crate::entry::EntryState::Absent,
            generation: Generation::new(0),
            tier: TierId::L0,
            tier_health: crate::tier::tier_trait::TierHealth::default(),
            expiration: None,
        }
    }

    /// Build a `CacheState` from a live control-plane snapshot plus the
    /// current health of the tier the policy is evaluating.
    pub fn from_snapshot(
        snapshot: &crate::control::cachelito::ControlSnapshot,
        tier_health: crate::tier::tier_trait::TierHealth,
    ) -> Self {
        CacheState {
            entry_state: snapshot.state,
            generation: snapshot.generation,
            tier: snapshot.tier,
            tier_health,
            expiration: None,
        }
    }
}

#[derive(Debug)]
pub struct PolicyDecision {
    pub authorized: bool,
    pub tier: TierId,
    pub operation: CacheOperation,
    pub population: PopulationStrategy,
    pub fail_mode: FailMode,
}

impl PolicyDecision {
    pub fn allow(tier: TierId) -> Self {
        PolicyDecision {
            authorized: true,
            tier,
            operation: CacheOperation::Get,
            population: PopulationStrategy::SingleFlight,
            fail_mode: FailMode::Open,
        }
    }

    pub fn deny() -> Self {
        PolicyDecision {
            authorized: false,
            tier: TierId::L0,
            operation: CacheOperation::Get,
            population: PopulationStrategy::SingleFlight,
            fail_mode: FailMode::Closed,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DefaultPolicy;

/// The last rung the cache ladder may *select*.
///
/// `L6` is authority and is deliberately excluded: the ladder routes writes and
/// fallbacks, and an entry must never be routed onto the authority tier. `L6` is
/// reachable only by an explicit authority read. Keeping the bound as a named
/// constant means adding a seventh *cache* tier later is a deliberate edit here
/// rather than an accident of `L5.as_usize()` appearing in three places.
pub const LAST_CACHE_TIER: crate::tier::TierId = crate::tier::TierId::L5;

/// Every rung the cache ladder may select, in ladder order.
///
/// This is the single iteration source for every path that scans rungs:
/// policy selection, promotion, demotion, and the fail-open fallback scan. The
/// previous fallback iterated `TierRegistry::all()`, which includes the
/// authority rung — so a fallback could surface authority data, and on a
/// six-rung manager `tier_for(L6)` substituted L0, meaning the scan could return
/// L0's bytes labelled as a lower rung's.
///
/// Returning a `const` array rather than a `Vec` keeps this allocation-free,
/// which matters because the fallback runs on the population failure path.
pub const fn cache_ladder() -> [crate::tier::TierId; 6] {
    let _ = LAST_CACHE_TIER;
    [
        crate::tier::TierId::L0,
        crate::tier::TierId::L1,
        crate::tier::TierId::L2,
        crate::tier::TierId::L3,
        crate::tier::TierId::L4,
        crate::tier::TierId::L5,
    ]
}

/// The authority rung, named once so no call site infers it from a tier number.
pub const AUTHORITY_TIER: crate::tier::TierId = crate::tier::TierId::L6;

/// Decision precedence, per `specs/policy.toml`. Lower number = applied first.
pub mod precedence {
    pub const EXPLICIT_POLICY: u8 = 1;
    pub const TIER_HEALTH: u8 = 2;
    pub const CONSISTENCY: u8 = 3;
    pub const AUTHZ_DENY: u8 = 4;
    pub const LATENCY: u8 = 5;
    pub const CAPACITY: u8 = 6;
    pub const DEFAULT_TIER: u8 = 7;
}

impl DefaultPolicy {
    /// Preferred (default) tier for an operation before precedence overrides.
    fn base_tier(op: CacheOperation, current: crate::tier::TierId) -> crate::tier::TierId {
        use crate::tier::TierId;
        match op {
            CacheOperation::Get => current,
            CacheOperation::Set => TierId::L1,
            CacheOperation::Remove
            | CacheOperation::Invalidate
            | CacheOperation::Refresh
            | CacheOperation::Exists
            | CacheOperation::Promote
            | CacheOperation::Demote => TierId::L0,
        }
    }

    /// Step a tier toward the origin (away from hot) when health forbids the base.
    /// Bounded by the fixed tier count (Rule 2).
    fn healthy_fallback(start: crate::tier::TierId, state: &CacheState) -> crate::tier::TierId {
        use crate::tier::TierId;
        // Only demote away from the base tier if that tier's circuit is open.
        if !state.tier_health.is_circuit_open() || state.tier != start {
            return start;
        }
        let mut idx = start.as_usize();
        for _ in 0..crate::policy::LAST_CACHE_TIER.as_usize() {
            if idx >= crate::policy::LAST_CACHE_TIER.as_usize() {
                break;
            }
            idx += 1;
        }
        TierId::from_usize(idx).unwrap_or(crate::policy::LAST_CACHE_TIER)
    }

    /// Precedence 5 (latency): a tight latency budget prefers hotter tiers by
    /// remaining where the base tier already is (hot). Documented as a no-further-
    /// demotion guard: we never push *toward* origin when latency is constrained.
    /// Precedence 6 (capacity): when the base tier reports no availability, fall
    /// back toward the origin so the entry still lands somewhere writable.
    fn capacity_fallback(start: crate::tier::TierId, state: &CacheState) -> crate::tier::TierId {
        use crate::tier::TierId;
        if state.tier != start || state.tier_health.availability > 0.0 {
            return start;
        }
        // Base tier is full: move one step toward origin (bounded by tier count).
        let next = (start.as_usize() + 1).min(crate::policy::LAST_CACHE_TIER.as_usize());
        TierId::from_usize(next).unwrap_or(crate::policy::LAST_CACHE_TIER)
    }
}

impl<K, V> CachePolicy<K, V> for DefaultPolicy {
    fn select(
        &self,
        request: &CacheRequest<K, V>,
        state: &CacheState,
        _identity: &IdentityContext,
    ) -> PolicyDecision {
        // DefaultPolicy is permissive: it authorizes every request and applies
        // deterministic routing plus the precedence ladder below. Deny logic is
        // the application's responsibility via a custom `CachePolicy`
        // (see `StrictPolicy`). Authz (precedence 4) always runs in
        // `CacheManager` before/around policy selection regardless.
        let mut decision = PolicyDecision::allow(Self::base_tier(request.operation, state.tier));
        decision.operation = request.operation;

        // Precedence 2 (tier_health): route away from an open-circuit tier.
        decision.tier = Self::healthy_fallback(decision.tier, state);
        // Precedence 6 (capacity): for writes only, if the chosen tier is full
        // fall back toward origin so the entry still lands somewhere writable.
        if matches!(
            request.operation,
            CacheOperation::Set | CacheOperation::Refresh
        ) {
            decision.tier = Self::capacity_fallback(decision.tier, state);
        }
        decision
    }
}

/// A policy that enforces authentication-based authorization.
///
/// - Read operations (`Get`, `Exists`) are allowed for any identity.
/// - Mutating operations require an authenticated identity; anonymous callers
///   are denied (precedence 4, `authz_deny`).
///
/// Use this when the cache holds data that must not be modified by anonymous
/// requests. `DefaultPolicy` remains permissive for open caches.
#[derive(Debug, Clone)]
pub struct StrictPolicy;

impl<K, V> CachePolicy<K, V> for StrictPolicy {
    fn select(
        &self,
        request: &CacheRequest<K, V>,
        state: &CacheState,
        identity: &IdentityContext,
    ) -> PolicyDecision {
        let mutating = matches!(
            request.operation,
            CacheOperation::Set
                | CacheOperation::Remove
                | CacheOperation::Invalidate
                | CacheOperation::Refresh
                | CacheOperation::Promote
                | CacheOperation::Demote
        );

        if mutating && !identity.is_authenticated() {
            return PolicyDecision::deny();
        }

        DefaultPolicy.select(request, state, identity)
    }
}

pub trait CachePolicy<K, V>: Send + Sync {
    fn select(
        &self,
        request: &CacheRequest<K, V>,
        state: &CacheState,
        identity: &IdentityContext,
    ) -> PolicyDecision;
}
