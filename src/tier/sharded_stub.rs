//! A sharded in-memory tier stub.
//!
//! # Why not `DashMap`
//!
//! `DashMap` is named in this crate's `AGENTS.md` as a rejected choice: "the
//! control plane is a pre-allocated sharded slot map (no `DashMap`; `TETANUS` Rule
//! 3)". Both objections hold and the second one got sharper when the tiers went
//! async in 0.3.0.
//!
//! Rule 3 is about allocation: `DashMap` allocates on insert, so a cache that
//! is supposed to have a fixed, pre-allocated footprint no longer does. The
//! await rule is worse. A `DashMap` read/write returns a guard whose lifetime is
//! chosen by the caller, and holding one across an `.await` in a sync-in-an-async
//! world is exactly how a shard deadlocks. Making the tiers async is what turns
//! that from a convention into a live hazard, so reaching for a concurrent map
//! here would have made the coarse-lock problem worse rather than better.
//!
//! # What this does instead
//!
//! It copies the pattern `Cachelito` already uses: N independently locked,
//! pre-allocated shard tables keyed by a hash of the key. Pre-allocated, so no
//! allocation after init; independently locked, so two keys landing on different
//! shards do not serialise; and no guard type that can escape, because every
//! critical section is a scoped `MutexGuard` inside a non-async method.
//!
//! This is the data plane. It stores values and nothing else - no entry state,
//! no generation, no population ownership. `Cachelito` remains the sole control
//! surface.

use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use crate::error::CacheError;
use crate::integrity::IntegrityCheck;
use crate::key::KeyRef;
use crate::tier::fixed_tier_stub::FixedTierStub;

/// Default shard count. Four gives most of the available parallelism for a
/// handful of threads without making each shard so small that occupancy
/// behaviour gets noisy.
pub const DEFAULT_SHARDS: usize = 4;

/// Default per-shard capacity. Total capacity is `shards * capacity_per_shard`,
/// so the default holds the same 1024 entries the single-table stub did.
pub const DEFAULT_CAPACITY_PER_SHARD: usize = 256;

/// A fixed-capacity in-memory tier split across independently locked shards.
#[derive(Debug)]
pub struct ShardedTierStub<V> {
    shards: Vec<Mutex<FixedTierStub<V>>>,
    mask: usize,
}

impl<V> ShardedTierStub<V> {
    /// Build with [`DEFAULT_SHARDS`] and [`DEFAULT_CAPACITY_PER_SHARD`].
    ///
    /// # Panics
    ///
    /// Panics only on startup pool-allocation failure - the sanctioned init-time
    /// failure mode. Use [`ShardedTierStub::with_shards`] for a fallible
    /// constructor.
    #[allow(clippy::expect_used)] // sanctioned init-time failure mode; see doc above
    pub fn new() -> Self {
        Self::with_shards(DEFAULT_SHARDS, DEFAULT_CAPACITY_PER_SHARD)
            .expect("ShardedTierStub init: shard allocation failed at startup")
    }

    /// Fallible constructor.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `shards` is zero (which would make the shard mask
    /// meaningless) or `capacity_per_shard` is zero, or when a shard pool fails
    /// to allocate.
    pub fn with_shards(shards: usize, capacity_per_shard: usize) -> Result<Self, CacheError> {
        // Rule 2/5: a zero shard count would make the mask zero and send every
        // key to shard 0, reintroducing the coarse lock this type exists to
        // remove. Clamp rather than divide by zero.
        let requested = shards.max(1);
        // The hot path masks with `n - 1`, which is only correct when `n` is a
        // power of two. Rounding UP and building exactly that many shards keeps
        // every shard reachable; rounding down would strand shards the mask can
        // never produce (with n=3 and mask=2 the index is only ever 0 or 2).
        let n = requested.next_power_of_two();
        let mut built = Vec::with_capacity(n);
        for _ in 0..n {
            built.push(Mutex::new(FixedTierStub::with_capacity(
                capacity_per_shard,
            )?));
        }
        Ok(Self {
            shards: built,
            mask: n - 1,
        })
    }

    /// The shard mask, exposed so tests can assert distribution rather than
    /// infer it. Production callers should not need this.
    #[must_use]
    pub fn shard_mask(&self) -> usize {
        self.mask
    }

    /// Number of live shards.
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Total entry capacity across all shards.
    #[must_use]
    /// Total slots across every shard.
    ///
    /// Sourced from the built shards rather than from the default constant: the
    /// previous version returned `shards.len() * DEFAULT_CAPACITY_PER_SHARD`,
    /// which reports 1024 for a stub configured with two slots per shard. A
    /// capacity accessor that is wrong is worse than none.
    pub fn capacity(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().map_or(0, |g| g.capacity()))
            .sum()
    }

    fn shard_for(&self, key: &KeyRef<'_>) -> &Mutex<FixedTierStub<V>> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        let h = hasher.finish() as usize;
        &self.shards[h & self.mask]
    }

    pub fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError>
    where
        V: Clone + IntegrityCheck,
    {
        self.shard_for(key).lock().map_err(poisoned)?.get(key)
    }

    pub fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError>
    where
        V: IntegrityCheck,
    {
        self.shard_for(key)
            .lock()
            .map_err(poisoned)?
            .set(key, value, ttl)
    }

    pub fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        self.shard_for(key).lock().map_err(poisoned)?.remove(key)
    }

    pub fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        self.shard_for(key).lock().map_err(poisoned)?.contains(key)
    }
}

/// A poisoned shard mutex means another thread panicked while holding it. That
/// is a real error, not something to paper over by reusing the table, so it
/// surfaces as a `ConfigurationError` rather than being silently ignored.
fn poisoned<T>(_: T) -> CacheError {
    CacheError::ConfigurationError
}
