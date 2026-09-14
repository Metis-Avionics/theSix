#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TierId {
    L0,
    L1,
    L2,
    L3,
    L4,
    L5,
}

impl TierId {
    pub fn as_usize(&self) -> usize {
        match self {
            TierId::L0 => 0,
            TierId::L1 => 1,
            TierId::L2 => 2,
            TierId::L3 => 3,
            TierId::L4 => 4,
            TierId::L5 => 5,
        }
    }

    pub fn from_usize(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(TierId::L0),
            1 => Some(TierId::L1),
            2 => Some(TierId::L2),
            3 => Some(TierId::L3),
            4 => Some(TierId::L4),
            5 => Some(TierId::L5),
            _ => None,
        }
    }
}

impl std::fmt::Display for TierId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TierId::L0 => write!(f, "L0"),
            TierId::L1 => write!(f, "L1"),
            TierId::L2 => write!(f, "L2"),
            TierId::L3 => write!(f, "L3"),
            TierId::L4 => write!(f, "L4"),
            TierId::L5 => write!(f, "L5"),
        }
    }
}

#[derive(Debug)]
pub struct TierRegistry {
    tiers: Vec<TierId>,
    health: Vec<std::sync::RwLock<TierHealth>>,
}

impl TierRegistry {
    pub fn new() -> Self {
        // Fixed tier set; no fallible operation, so no unwrap is needed.
        let tiers: Vec<TierId> = vec![
            TierId::L0,
            TierId::L1,
            TierId::L2,
            TierId::L3,
            TierId::L4,
            TierId::L5,
        ];
        let health = (0..6)
            .map(|_| std::sync::RwLock::new(TierHealth::default()))
            .collect();
        TierRegistry { tiers, health }
    }

    pub fn all(&self) -> &[TierId] {
        &self.tiers
    }

    pub fn len(&self) -> usize {
        self.tiers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    pub fn tier_health(&self, tier: TierId) -> TierHealth {
        self.health[tier.as_usize()]
            .read()
            .map(|h| h.clone())
            .unwrap_or_default()
    }

    /// Record a failure against a tier; increments the consecutive-failure
    /// counter and degrades the health score. Used by the circuit breaker.
    pub fn fail(&self, tier: TierId) {
        if let Ok(mut h) = self.health[tier.as_usize()].write() {
            h.consecutive_failures += 1;
            h.last_failure_timestamp = Some(std::time::SystemTime::now());
            h.health_score = (h.health_score - 0.1).max(0.0);
        }
    }

    /// Reset a tier's health after a successful operation.
    pub fn recover(&self, tier: TierId) {
        if let Ok(mut h) = self.health[tier.as_usize()].write() {
            h.consecutive_failures = 0;
            h.health_score = 1.0;
            h.last_failure_timestamp = None;
        }
    }

    pub fn is_circuit_open(&self, tier: TierId) -> bool {
        self.health[tier.as_usize()]
            .read()
            .map_or(true, |h| h.is_circuit_open())
    }

    pub fn try_fallback_tier(&self, exclude: TierId) -> Option<TierId> {
        for &tier in &self.tiers {
            if tier == exclude {
                continue;
            }
            if !self.is_circuit_open(tier) {
                return Some(tier);
            }
        }
        None
    }
}

impl Default for TierRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub mod fixed_tier_stub;
pub mod l0;
pub mod l1;
pub mod l2;
pub mod l3;
pub mod l4;
pub mod l5;
pub mod test;

pub mod backends;

#[path = "trait.rs"]
pub mod tier_trait;

pub use tier_trait::{CacheTier, TierHealth};
