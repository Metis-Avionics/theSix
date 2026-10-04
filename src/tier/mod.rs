#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TierId {
    L0,
    L1,
    L2,
    L3,
    L4,
    L5,
    /// Authority. Not a cache rung: it is never blind-written and never
    /// invalidated. Writes reach it through the owning repository, which then
    /// invalidates downward.
    L6,
}

impl TierId {
    /// Every rung, in ladder order. `capabilities()` iterates this so a new
    /// rung cannot be forgotten by a hand-written list.
    pub const ALL: [TierId; 7] = [
        TierId::L0,
        TierId::L1,
        TierId::L2,
        TierId::L3,
        TierId::L4,
        TierId::L5,
        TierId::L6,
    ];

    pub const fn as_usize(&self) -> usize {
        match self {
            TierId::L0 => 0,
            TierId::L1 => 1,
            TierId::L2 => 2,
            TierId::L3 => 3,
            TierId::L4 => 4,
            TierId::L5 => 5,
            TierId::L6 => 6,
        }
    }

    pub const fn from_usize(idx: usize) -> Option<Self> {
        match idx {
            0 => Some(TierId::L0),
            1 => Some(TierId::L1),
            2 => Some(TierId::L2),
            3 => Some(TierId::L3),
            4 => Some(TierId::L4),
            5 => Some(TierId::L5),
            6 => Some(TierId::L6),
            _ => None,
        }
    }

    /// Whether this rung participates in the cache ladder.
    ///
    /// The authority rung is not a rung. Policy selection, promotion, demotion
    /// and the fail-open fallback all ask this rather than comparing against a
    /// literal, so adding a rung above the authority cannot silently widen any
    /// scan.
    #[must_use]
    pub const fn is_cache_rung(self) -> bool {
        self.as_usize() <= LAST_CACHE_RUNG_INDEX
    }

    /// Whether this rung is the configured authority.
    ///
    /// Reads the same constant the contract's `[authority.l6]` names. Authority
    /// is a configured role, not something inferred from a tier's position — the
    /// distinction matters because the previous code compared against
    /// `TierId::L6` in three places by two different mechanisms, one of which
    /// indexed the caller's tier vector and so misfired on a reordered vec.
    #[must_use]
    pub const fn is_authority_rung(self) -> bool {
        self.as_usize() == AUTHORITY_RUNG_INDEX
    }

    /// The discriminant, for storage in a fixed-size record.
    ///
    /// The control plane stores rung ids as `u8` so a commit intent stays
    /// allocation-free and `Copy`; that is only safe if the mapping is total.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.as_usize() as u8
    }

    /// Inverse of [`Self::as_u8`]. Returns `None` for a byte that is not a rung.
    #[must_use]
    pub const fn from_index(index: u8) -> Option<Self> {
        if (index as usize) < 7 {
            Self::from_usize(index as usize)
        } else {
            None
        }
    }

    /// Parse the `"L3"` spelling used by `TierId::Display` and by the contract.
    ///
    /// Exists so the contract's `last_cache_rung = "L5"` can be checked against
    /// the code's `LAST_CACHE_TIER` rather than being a second, unverified
    /// spelling of the same bound.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix('L')?;
        let idx: usize = rest.parse().ok()?;
        Self::from_usize(idx)
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
            TierId::L6 => write!(f, "L6"),
        }
    }
}

/// Index of the last rung in the cache ladder. Mirrors
/// [`crate::policy::LAST_CACHE_TIER`]; the contract gate asserts the two agree.
pub const LAST_CACHE_RUNG_INDEX: usize = 5;

/// Index of the authority rung.
pub const AUTHORITY_RUNG_INDEX: usize = 6;

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
            TierId::L6,
        ];
        // One health slot per tier. Sized from the tier list rather than a
        // literal so adding a rung cannot desynchronise the two.
        let health = (0..tiers.len())
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
pub mod sharded_stub;
pub mod test;

pub mod backends;

#[path = "trait.rs"]
pub mod tier_trait;

pub use tier_trait::{BackendKind, CacheTier, TierHealth};
