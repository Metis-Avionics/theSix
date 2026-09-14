use crate::error::CacheError;
use crate::key::KeyRef;
use crate::pool::MemoryPool;

const DEFAULT_CAPACITY: usize = 1024;

#[derive(Debug)]
pub struct FixedTierStub<V> {
    pool: MemoryPool<V>,
    slots: Vec<Option<Slot>>,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    key_hash: u64,
    pool_idx: usize,
    /// When the entry was written and its TTL. `None` TTL = never expires.
    expiry: Option<(std::time::Instant, std::time::Duration)>,
}

impl Slot {
    fn is_expired(&self) -> bool {
        match self.expiry {
            Some((armed, ttl)) => armed.elapsed() >= ttl,
            None => false,
        }
    }
}

impl<V> FixedTierStub<V> {
    /// Create a stub with default capacity.
    ///
    /// # Panics
    /// Panics only if the process cannot allocate the fixed-capacity pool at
    /// startup (allocation failure or zero default capacity). This is the
    /// TETANUS-sanctioned init-time failure mode: construction is infallible
    /// for valid configurations and only ever fails before any data-plane work.
    /// Use [`FixedTierStub::with_capacity`] for a fallible constructor.
    #[allow(clippy::expect_used)] // sanctioned init-time failure mode; see doc above
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
            .expect("FixedTierStub init: pool allocation failed at startup")
    }

    /// Fallible constructor. Returns `Err(CacheError::ConfigurationError)` when
    /// `capacity` is zero, or propagates pool-allocation failure.
    pub fn with_capacity(capacity: usize) -> Result<Self, CacheError> {
        if capacity == 0 {
            return Err(CacheError::ConfigurationError);
        }
        let pool = MemoryPool::new(capacity)?;
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(None);
        }
        Ok(FixedTierStub { pool, slots })
    }

    fn hash_key(key: &KeyRef<'_>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    /// Find the slot index holding `key_hash`, treating expired entries as
    /// absent. Bounded by `slots.len()` (Rule 2).
    pub fn find_slot(&self, key_hash: u64) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mut idx = (key_hash as usize) % self.slots.len();
        let mut attempts = 0;
        while attempts < self.slots.len() {
            match &self.slots[idx] {
                Some(s) if s.key_hash == key_hash => {
                    if s.is_expired() {
                        return None;
                    }
                    return Some(idx);
                }
                None => return None,
                _ => {
                    idx = (idx + 1) % self.slots.len();
                    attempts += 1;
                }
            }
        }
        None
    }

    /// Find a slot suitable for (re)writing `key_hash`: an empty slot, the
    /// existing slot for this key, or an expired slot (which we may reuse).
    pub fn find_empty(&self, key_hash: u64) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mut idx = (key_hash as usize) % self.slots.len();
        let mut attempts = 0;
        while attempts < self.slots.len() {
            match &self.slots[idx] {
                None => return Some(idx),
                Some(s) if s.key_hash == key_hash => return Some(idx),
                Some(s) if s.is_expired() => return Some(idx),
                _ => {
                    idx = (idx + 1) % self.slots.len();
                    attempts += 1;
                }
            }
        }
        None
    }

    pub fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError>
    where
        V: Clone,
    {
        let hash = Self::hash_key(key);
        if let Some(idx) = self.find_slot(hash) {
            if let Some(slot) = self.slots[idx] {
                return Ok(self.pool.get(slot.pool_idx).cloned());
            }
        }
        Ok(None)
    }

    pub fn set(
        &mut self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        let hash = Self::hash_key(key);
        if let Some(idx) = self.find_empty(hash) {
            // Reuse the slot's existing pool index when overwriting (Rule 3:
            // no new allocation needed when the slot is already populated).
            let pool_idx = match self.slots[idx] {
                Some(old) => {
                    if let Some(v) = self.pool.get_mut(old.pool_idx) {
                        *v = value;
                        old.pool_idx
                    } else {
                        self.pool.allocate(value)?
                    }
                }
                None => self.pool.allocate(value)?,
            };
            self.slots[idx] = Some(Slot {
                key_hash: hash,
                pool_idx,
                expiry: ttl.map(|t| (std::time::Instant::now(), t)),
            });
            Ok(())
        } else {
            Err(CacheError::ConfigurationError)
        }
    }

    pub fn remove(&mut self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        let hash = Self::hash_key(key);
        if let Some(idx) = self.find_slot(hash) {
            if let Some(slot) = self.slots[idx].take() {
                // Slot index is always in-range here; dealloc cannot fail.
                let _ = self.pool.deallocate(slot.pool_idx);
            }
        }
        Ok(())
    }

    pub fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        let hash = Self::hash_key(key);
        Ok(self.find_slot(hash).is_some())
    }
}
