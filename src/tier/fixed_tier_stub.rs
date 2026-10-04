use crate::error::CacheError;
use crate::integrity::{ContentDigest, IntegrityCheck, KeyAddress, KeyFingerprint, Placement};
use crate::key::KeyRef;
use crate::pool::MemoryPool;

const DEFAULT_CAPACITY: usize = 1024;

/// Fixed-size storage for one rung: a pre-allocated value pool plus an
/// open-addressed index.
///
/// # Integrity
///
/// Each slot carries two independent pieces of metadata:
///
/// * a [`KeyFingerprint`] over the full key bytes, so a placement collision
///   between two distinct keys is *detected* rather than silently aliasing one
///   onto the other, and
/// * a [`ContentDigest`] over the value, so a damaged read is reported as
///   `Corrupted` instead of being served.
///
/// Neither is cryptographic; see [`crate::integrity`] for what that does and does
/// not claim.
#[derive(Debug)]
pub struct FixedTierStub<V> {
    pool: MemoryPool<V>,
    slots: Vec<Option<Slot>>,
    placement: Placement,
    /// Counts reads rejected by the content digest. Observable so a test can
    /// prove the integrity check actually fired rather than assuming it.
    corruptions_detected: std::sync::atomic::AtomicU64,
    /// Where the next eviction scan starts (B17).
    ///
    /// Without this, `eviction_candidate` would always nominate the lowest
    /// occupied slot index; that slot is freed, immediately refilled by the
    /// write that triggered the eviction, and chosen again next time. The tier
    /// would thrash a single entry while the rest of the table stayed full.
    /// Rotating the start point makes eviction round-robin over the table,
    /// which is deterministic (no clock, no RNG) and testable.
    eviction_cursor: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    /// Where to probe from. Placement only: two keys may share it harmlessly.
    placement: u64,
    /// Who this slot belongs to. Compared in full, so a placement collision
    /// cannot alias two keys.
    fingerprint: KeyFingerprint,
    /// Digest of the stored value, checked on read.
    digest: ContentDigest,
    pool_idx: usize,
    /// When the entry was written and its TTL. `None` TTL = never expires.
    expiry: Option<(std::time::Instant, std::time::Duration)>,
    /// Control-plane address of the key in this slot (B17).
    ///
    /// Held so a slot can be identified for eviction without retaining the
    /// key: the confidentiality invariant forbids keeping key material, so
    /// eviction addresses slots, never keys.
    address: KeyAddress,
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
        Ok(FixedTierStub {
            pool,
            slots,
            placement: Placement::Default,
            corruptions_detected: std::sync::atomic::AtomicU64::new(0),
            eviction_cursor: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// The same stub, reading keys with a different placement strategy.
    ///
    /// Test-only in practice. Injecting `Placement::CollidingPair` is the only
    /// way to demonstrate that two colliding keys still do not alias, which is
    /// the difference between testing `no_cross_key_corruption` and asserting it.
    #[must_use]
    pub fn with_placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// How many reads the content digest rejected.
    #[must_use]
    pub fn corruptions_detected(&self) -> u64 {
        self.corruptions_detected
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Slots this table holds.
    ///
    /// Read by the soak test to assert the table does not grow under load, which
    /// is the only way a fixed-capacity claim is checkable from outside.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    fn placement_of(&self, key: &KeyRef<'_>) -> u64 {
        self.placement.hash(key.0)
    }

    fn fingerprint_of(key: &KeyRef<'_>) -> KeyFingerprint {
        KeyFingerprint::of(key.0)
    }

    /// Find the slot index holding `key_hash`, treating expired entries as
    /// absent. Bounded by `slots.len()` (Rule 2).
    pub fn find_slot(&self, placement: u64, fingerprint: KeyFingerprint) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mut idx = (placement as usize) % self.slots.len();
        let mut attempts = 0;
        while attempts < self.slots.len() {
            match &self.slots[idx] {
                // Full-fingerprint comparison: a slot whose placement matches but
                // whose key does not is somebody else's entry, and stopping here
                // would alias two keys onto one pool index.
                Some(s) if s.placement == placement && s.fingerprint == fingerprint => {
                    if s.is_expired() {
                        return None;
                    }
                    return Some(idx);
                }
                // An empty slot is *not* proof of absence, and this is the second
                // way deletion has to be handled.
                //
                // Open addressing resolves collisions by probing forward, so a key
                // may sit several slots past its home. Removing an earlier key
                // punches a hole in the middle of that probe chain. Treating the
                // hole as "not here" orphaned everything behind it: `get` returned
                // `None` for values still in the pool, and the next `set` inserted
                // a second copy, leaking a pool index each time. A soak over
                // fill/reclaim cycles ran the table out of capacity while it was
                // only two-thirds full.
                //
                // So a lookup scans the whole table. Tombstones would restore the
                // early exit, at the cost of a per-slot marker and a compaction
                // story; the table is fixed and small, so the scan is cheaper than
                // the bookkeeping and cannot be wrong.
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
    pub fn find_empty(&self, placement: u64, fingerprint: KeyFingerprint) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mut idx = (placement as usize) % self.slots.len();
        let mut attempts = 0;
        while attempts < self.slots.len() {
            match &self.slots[idx] {
                None => return Some(idx),
                Some(s) if s.placement == placement && s.fingerprint == fingerprint => {
                    return Some(idx);
                }
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
        V: Clone + IntegrityCheck,
    {
        let placement = self.placement_of(key);
        let fingerprint = Self::fingerprint_of(key);
        let Some(idx) = self.find_slot(placement, fingerprint) else {
            return Ok(None);
        };
        let Some(slot) = self.slots[idx] else {
            return Ok(None);
        };
        let Some(value) = self.pool.get(slot.pool_idx) else {
            return Ok(None);
        };
        // The integrity gate. A mismatch means the stored bytes are not what was
        // written, so the value is refused rather than served.
        if value.content_digest() != slot.digest {
            self.corruptions_detected
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Err(CacheError::Corrupted);
        }
        Ok(Some(value.clone()))
    }

    pub fn set(
        &mut self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError>
    where
        V: IntegrityCheck,
    {
        let placement = self.placement_of(key);
        let fingerprint = Self::fingerprint_of(key);
        let digest = value.content_digest();
        if let Some(idx) = self.find_empty(placement, fingerprint) {
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
                placement,
                fingerprint,
                digest,
                pool_idx,
                expiry: ttl.map(|t| (std::time::Instant::now(), t)),
                address: KeyAddress::of(key.0, self.placement),
            });
            Ok(())
        } else {
            // The slot table is full. That is a runtime condition under load,
            // not a misconfiguration, so it gets its own variant rather than
            // being reported as one.
            Err(CacheError::CapacityExhausted)
        }
    }

    pub fn remove(&mut self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        let placement = self.placement_of(key);
        let fingerprint = Self::fingerprint_of(key);
        if let Some(idx) = self.find_slot(placement, fingerprint)
            && let Some(slot) = self.slots[idx].take()
        {
            // Slot index is always in-range here; dealloc cannot fail.
            let _ = self.pool.deallocate(slot.pool_idx);
        }
        Ok(())
    }

    pub fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        let placement = self.placement_of(key);
        let fingerprint = Self::fingerprint_of(key);
        Ok(self.find_slot(placement, fingerprint).is_some())
    }

    /// Replace a stored value's bytes without updating its digest.
    ///
    /// The only way to produce a genuinely corrupt record: every other path
    /// writes the value and its digest together, so a test that wants to prove
    /// the integrity check works has to damage the store behind its back. This
    /// models exactly that — memory that changed under the process — and nothing
    /// else.
    ///
    /// Test-only by intent. It is not gated behind a feature because it is inert
    /// unless called, and gating it would mean the corruption tests could not run
    /// under `--all-features` without also shipping the harness.
    pub fn corrupt_stored_value_for_test(
        &mut self,
        key: &KeyRef<'_>,
        replacement: V,
    ) -> Result<bool, CacheError>
    where
        V: Clone,
    {
        let placement = self.placement_of(key);
        let fingerprint = Self::fingerprint_of(key);
        let Some(idx) = self.find_slot(placement, fingerprint) else {
            return Ok(false);
        };
        let Some(slot) = self.slots[idx] else {
            return Ok(false);
        };
        if let Some(v) = self.pool.get_mut(slot.pool_idx) {
            *v = replacement;
        }
        Ok(true)
    }
}

impl<V> FixedTierStub<V> {
    /// Nominate the next slot for eviction, scanning round-robin from a
    /// rotating cursor.
    ///
    /// Deterministic by construction: no clock and no RNG, so a test can assert
    /// exactly which entry a full table gives up. The cursor advances past the
    /// nominated slot so consecutive evictions walk the whole table instead of
    /// thrashing one entry (see `eviction_cursor`).
    ///
    /// Expired slots are skipped. `find_empty` already reclaims those on the
    /// write path, so reaching here means the survivors are live -- but a
    /// direct call may still observe one, and returning an expired entry as a
    /// victim would waste an eviction the control plane has to authorise.
    pub fn eviction_candidate(&mut self) -> Option<KeyAddress> {
        let n = self.slots.len();
        if n == 0 {
            return None;
        }
        let start = self
            .eviction_cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % n;
        for offset in 0..n {
            let idx = (start + offset) % n;
            match &self.slots[idx] {
                Some(s) if !s.is_expired() => {
                    self.eviction_cursor
                        .store(idx + 1, std::sync::atomic::Ordering::Relaxed);
                    return Some(s.address);
                }
                _ => {}
            }
        }
        // Every slot is expired. Nominate the first one anyway: it is dead, so
        // the control plane authorising its eviction costs nothing.
        self.slots.iter().flatten().next().map(|s| s.address)
    }

    /// Remove the slot holding `address`, only if it still holds it.
    ///
    /// Conditional on purpose. The control plane authorises an eviction by
    /// advancing the generation, but the slot may be refilled before this runs;
    /// an unconditional remove would then delete a value the control plane has
    /// since committed. Returning `false` is safe because the reservation
    /// already invalidated whatever was there.
    pub fn remove_if_address(&mut self, address: KeyAddress) -> Result<bool, CacheError> {
        let n = self.slots.len();
        for idx in 0..n {
            if let Some(slot) = &self.slots[idx]
                && slot.address == address
            {
                let taken = self.slots[idx].take();
                if let Some(taken) = taken {
                    // Slot index is in-range by construction; dealloc cannot fail.
                    let _ = self.pool.deallocate(taken.pool_idx);
                }
                return Ok(true);
            }
        }
        Ok(false)
    }
}
