use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;

use crate::entry::{EntryState, Generation};
use crate::error::CacheError;
use crate::tier::TierId;
use crate::tier::tier_trait::TierHealth;

const DEFAULT_SHARD_COUNT: usize = 16;
const DEFAULT_CAPACITY_PER_SHARD: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryStateAtomic {
    Absent = 0,
    Ready = 1,
    Stale = 2,
    InFlight = 3,
    Failed = 4,
}

impl TryFrom<u8> for EntryStateAtomic {
    type Error = ();
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(EntryStateAtomic::Absent),
            1 => Ok(EntryStateAtomic::Ready),
            2 => Ok(EntryStateAtomic::Stale),
            3 => Ok(EntryStateAtomic::InFlight),
            4 => Ok(EntryStateAtomic::Failed),
            _ => Err(()),
        }
    }
}

impl From<EntryState> for EntryStateAtomic {
    fn from(s: EntryState) -> Self {
        match s {
            EntryState::Absent => EntryStateAtomic::Absent,
            EntryState::Ready => EntryStateAtomic::Ready,
            EntryState::Stale => EntryStateAtomic::Stale,
            EntryState::InFlight => EntryStateAtomic::InFlight,
            EntryState::Failed => EntryStateAtomic::Failed,
        }
    }
}

impl From<EntryStateAtomic> for EntryState {
    fn from(s: EntryStateAtomic) -> Self {
        match s {
            EntryStateAtomic::Absent => EntryState::Absent,
            EntryStateAtomic::Ready => EntryState::Ready,
            EntryStateAtomic::Stale => EntryState::Stale,
            EntryStateAtomic::InFlight => EntryState::InFlight,
            EntryStateAtomic::Failed => EntryState::Failed,
        }
    }
}

#[derive(Debug)]
pub struct ControlEntry {
    state: std::sync::atomic::AtomicU8,
    generation: std::sync::atomic::AtomicU64,
    tier: std::sync::atomic::AtomicU8,
    population_owner: std::sync::atomic::AtomicBool,
    population_timestamp: std::sync::atomic::AtomicU64,
    /// Absolute expiration instant stored as nanoseconds since the process
    /// clock anchor; 0 = no expiration. Never decremented, so any fixed
    /// reference works (we compare via `expiration_instant`).
    expiration_nanos: std::sync::atomic::AtomicU64,
    /// Discriminator of the terminal population error, set by `fail` so that
    /// waiting callers receive the owner's failure (stampede.toml
    /// `waiters_receive_population_error`). 0 = none.
    last_error: std::sync::atomic::AtomicU8,
    key_hash: u64,
    notify: Arc<Notify>,
}

impl ControlEntry {
    pub fn new(key_hash: u64) -> Self {
        ControlEntry {
            state: std::sync::atomic::AtomicU8::new(EntryStateAtomic::Absent as u8),
            generation: std::sync::atomic::AtomicU64::new(0),
            tier: std::sync::atomic::AtomicU8::new(TierId::L0 as u8),
            population_owner: std::sync::atomic::AtomicBool::new(false),
            population_timestamp: std::sync::atomic::AtomicU64::new(0),
            expiration_nanos: std::sync::atomic::AtomicU64::new(0),
            last_error: std::sync::atomic::AtomicU8::new(0),
            key_hash,
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn state(&self) -> EntryState {
        let raw = self.state.load(std::sync::atomic::Ordering::Acquire);
        EntryState::from(EntryStateAtomic::try_from(raw).unwrap_or(EntryStateAtomic::Absent))
    }

    pub fn set_state(&self, new_state: EntryState) {
        self.state.store(
            EntryStateAtomic::from(new_state) as u8,
            std::sync::atomic::Ordering::Release,
        );
    }

    pub fn generation(&self) -> Generation {
        Generation::new(self.generation.load(std::sync::atomic::Ordering::Acquire))
    }

    pub fn set_generation(&self, generation: Generation) {
        self.generation
            .store(generation.0, std::sync::atomic::Ordering::Release);
    }

    pub fn increment_generation(&self) -> Generation {
        let prev = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Generation::new(prev + 1)
    }

    pub fn tier(&self) -> TierId {
        TierId::from_usize(self.tier.load(std::sync::atomic::Ordering::Acquire) as usize)
            .unwrap_or(TierId::L0)
    }

    pub fn set_tier(&self, tier: TierId) {
        self.tier
            .store(tier.as_usize() as u8, std::sync::atomic::Ordering::Release);
    }

    pub fn is_population_owner(&self) -> bool {
        self.population_owner
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn set_population_owner(&self, owner: bool) {
        self.population_owner
            .store(owner, std::sync::atomic::Ordering::Release);
    }

    /// Monotonic anchor set at process start; used to encode `Instant` as
    /// nanoseconds since boot so timestamps round-trip without overflow.
    fn boot_anchor() -> Instant {
        use std::sync::OnceLock;
        static ANCHOR: OnceLock<Instant> = OnceLock::new();
        *ANCHOR.get_or_init(Instant::now)
    }

    fn instant_to_nanos(t: Instant) -> u64 {
        let anchor = Self::boot_anchor();
        let nanos = if t >= anchor {
            t.duration_since(anchor).as_nanos()
        } else {
            0
        };
        u64::try_from(nanos).unwrap_or(u64::MAX)
    }

    fn nanos_to_instant(nanos: u64) -> Option<Instant> {
        if nanos == 0 {
            None
        } else {
            Self::boot_anchor().checked_add(std::time::Duration::from_nanos(nanos))
        }
    }

    pub fn population_timestamp(&self) -> Option<Instant> {
        let ts = self
            .population_timestamp
            .load(std::sync::atomic::Ordering::Acquire);
        Self::nanos_to_instant(ts)
    }

    pub fn set_population_timestamp(&self, ts: Option<Instant>) {
        let nanos = ts.map_or(0, Self::instant_to_nanos);
        self.population_timestamp
            .store(nanos, std::sync::atomic::Ordering::Release);
    }

    pub fn notify(&self) {
        self.notify.notify_waiters();
    }

    pub fn notify_clone(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    pub fn key_hash(&self) -> u64 {
        self.key_hash
    }

    /// Record a TTL for the entry, marking when it was armed. `0` clears it.
    /// Expiry is derived from `population_timestamp` (the publish/populate
    /// time) plus this TTL, so no future `Instant` needs to be stored.
    pub fn set_ttl(&self, ttl: Option<std::time::Duration>) {
        let nanos = ttl.map_or(0, |d| d.as_nanos() as u64);
        self.expiration_nanos
            .store(nanos, std::sync::atomic::Ordering::Release);
    }

    pub fn ttl(&self) -> Option<std::time::Duration> {
        let nanos = self
            .expiration_nanos
            .load(std::sync::atomic::Ordering::Acquire);
        if nanos == 0 {
            None
        } else {
            Some(std::time::Duration::from_nanos(nanos))
        }
    }

    /// True when a TTL is armed and has elapsed since the entry was populated.
    pub fn is_expired(&self) -> bool {
        let (Some(ttl), Some(populated_at)) = (self.ttl(), self.population_timestamp()) else {
            return false;
        };
        populated_at.elapsed() >= ttl
    }

    /// Record the terminal population error so waiting callers can be told
    /// why the population failed. `None` clears the marker.
    pub fn set_last_error(&self, err: Option<CacheError>) {
        self.last_error.store(
            Self::encode_error(err),
            std::sync::atomic::Ordering::Release,
        );
    }

    /// The terminal population error recorded by `fail`, if any.
    pub fn last_error(&self) -> Option<CacheError> {
        Self::decode_error(self.last_error.load(std::sync::atomic::Ordering::Acquire))
    }

    fn encode_error(err: Option<CacheError>) -> u8 {
        match err {
            None => 0,
            Some(CacheError::Timeout) => 1,
            Some(CacheError::PopulationFailed) => 2,
            Some(CacheError::TierUnavailable) => 3,
            Some(CacheError::Cancelled) => 4,
            Some(_) => 2, // default to PopulationFailed for other variants
        }
    }

    fn decode_error(code: u8) -> Option<CacheError> {
        match code {
            1 => Some(CacheError::Timeout),
            2 => Some(CacheError::PopulationFailed),
            3 => Some(CacheError::TierUnavailable),
            4 => Some(CacheError::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ControlSnapshot {
    pub state: EntryState,
    pub generation: Generation,
    pub tier: TierId,
    pub population_owner: bool,
    pub population_timestamp: Option<Instant>,
    /// Time-to-live armed on this entry, if any.
    pub ttl: Option<std::time::Duration>,
    /// True when the entry's TTL has elapsed since it was populated.
    pub expired: bool,
    /// Terminal population error recorded by `fail`, if any.
    pub last_error: Option<CacheError>,
    pub notify: Arc<Notify>,
}

#[derive(Debug)]
struct Shard {
    entries: Vec<Option<ControlEntry>>,
}

impl Shard {
    pub fn new(capacity: usize) -> Self {
        let mut entries = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            entries.push(None);
        }
        Shard { entries }
    }

    fn find_slot(&self, key_hash: u64) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let mut idx = (key_hash as usize) % self.entries.len();
        let mut attempts = 0;
        while attempts < self.entries.len() {
            match &self.entries[idx] {
                Some(entry) if entry.key_hash() == key_hash => return Some(idx),
                None => return None,
                _ => {
                    idx = (idx + 1) % self.entries.len();
                    attempts += 1;
                }
            }
        }
        None
    }

    fn find_empty(&self, key_hash: u64) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let mut idx = (key_hash as usize) % self.entries.len();
        let mut attempts = 0;
        while attempts < self.entries.len() {
            match &self.entries[idx] {
                None => return Some(idx),
                Some(entry) if entry.key_hash() == key_hash => return Some(idx),
                _ => {
                    idx = (idx + 1) % self.entries.len();
                    attempts += 1;
                }
            }
        }
        None
    }

    fn find_or_create_entry(&mut self, key_hash: u64) -> Result<&mut ControlEntry, CacheError> {
        if let Some(idx) = self.find_slot(key_hash) {
            return self.entries[idx]
                .as_mut()
                .ok_or(CacheError::ConfigurationError);
        }
        if let Some(idx) = self.find_empty(key_hash) {
            self.entries[idx] = Some(ControlEntry::new(key_hash));
            return self.entries[idx]
                .as_mut()
                .ok_or(CacheError::ConfigurationError);
        }
        Err(CacheError::ConfigurationError)
    }

    fn find_entry(&self, key_hash: u64) -> Option<&ControlEntry> {
        let idx = self.find_slot(key_hash)?;
        self.entries[idx].as_ref()
    }
}

#[derive(Debug)]
pub struct Cachelito {
    shards: Vec<std::sync::Mutex<Shard>>,
    shard_count: usize,
    _capacity_per_shard: usize,
}

impl Cachelito {
    pub fn new() -> Self {
        Self::with_shards(DEFAULT_SHARD_COUNT)
    }

    pub fn with_shards(shard_count: usize) -> Self {
        // Rule 2/5: a zero shard count would cause modulo-by-zero in shard_for;
        // clamp to at least one shard (invalid configuration made safe).
        let shard_count = shard_count.max(1);
        let capacity_per_shard = DEFAULT_CAPACITY_PER_SHARD;
        let mut shards = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            shards.push(std::sync::Mutex::new(Shard::new(capacity_per_shard)));
        }
        Cachelito {
            shards,
            shard_count,
            _capacity_per_shard: capacity_per_shard,
        }
    }

    fn shard_for(&self, key: &[u8]) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) % self.shard_count
    }

    fn compute_key_hash(key: &[u8]) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    fn snapshot_of(
        entry: &ControlEntry,
        state: EntryState,
        generation: Generation,
        tier: TierId,
        population_owner: bool,
        population_timestamp: Option<Instant>,
    ) -> ControlSnapshot {
        ControlSnapshot {
            state,
            generation,
            tier,
            population_owner,
            population_timestamp,
            ttl: entry.ttl(),
            expired: entry.is_expired(),
            last_error: entry.last_error(),
            notify: entry.notify_clone(),
        }
    }

    pub fn acquire(&self, key: &[u8], tier: TierId) -> Result<ControlSnapshot, CacheError> {
        let key_hash = Self::compute_key_hash(key);
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let Ok(entry) = guard.find_or_create_entry(key_hash) else {
            return Err(CacheError::ConfigurationError);
        };

        let current_state = entry.state();
        let current_gen = entry.generation();
        let current_tier = entry.tier();

        match current_state {
            EntryState::Absent | EntryState::Failed | EntryState::Stale => {
                let new_state = EntryStateAtomic::InFlight as u8;
                // Claim from whichever claimable state we actually observed,
                // so Failed/Stale entries can be reclaimed for a retry.
                let expected_current = EntryStateAtomic::from(current_state) as u8;
                let result = entry.state.compare_exchange(
                    expected_current,
                    new_state,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                );

                if result.is_ok() {
                    entry.set_population_owner(true);
                    let new_gen = Generation::new(current_gen.0 + 1);
                    entry.set_generation(new_gen);
                    entry.set_tier(tier);
                    entry.set_last_error(None);
                    entry.set_population_timestamp(Some(Instant::now()));

                    Ok(Self::snapshot_of(
                        entry,
                        EntryState::InFlight,
                        new_gen,
                        tier,
                        true,
                        Some(Instant::now()),
                    ))
                } else if let Err(raw_err) = result {
                    let observed_state = EntryState::from(
                        EntryStateAtomic::try_from(raw_err).unwrap_or(EntryStateAtomic::Absent),
                    );
                    Ok(Self::snapshot_of(
                        entry,
                        observed_state,
                        current_gen,
                        current_tier,
                        false,
                        None,
                    ))
                } else {
                    Ok(Self::snapshot_of(
                        entry,
                        current_state,
                        current_gen,
                        current_tier,
                        false,
                        None,
                    ))
                }
            }
            EntryState::InFlight => Ok(Self::snapshot_of(
                entry,
                EntryState::InFlight,
                current_gen,
                current_tier,
                false,
                None,
            )),
            EntryState::Ready => Ok(Self::snapshot_of(
                entry,
                EntryState::Ready,
                current_gen,
                current_tier,
                false,
                None,
            )),
        }
    }

    pub fn publish(
        &self,
        key: &[u8],
        expected_generation: Generation,
        tier: TierId,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        let key_hash = Self::compute_key_hash(key);
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard.find_entry(key_hash).ok_or(CacheError::Miss)?;

        let current_gen = entry.generation();
        if expected_generation.is_stale(current_gen) || expected_generation.0 != current_gen.0 {
            return Err(CacheError::StaleGeneration);
        }

        entry.set_state(EntryState::Ready);
        entry.set_tier(tier);
        entry.set_ttl(ttl);
        entry.set_last_error(None);
        entry.set_population_owner(false);
        // Re-arm the populate timestamp so TTL is measured from publish time.
        entry.set_population_timestamp(Some(Instant::now()));
        entry.notify();

        Ok(())
    }

    pub fn fail(&self, key: &[u8]) -> Result<(), CacheError> {
        self.fail_with_error(key, CacheError::PopulationFailed)
    }

    /// Mark the entry failed, recording the terminal error so waiting callers
    /// learn why the population failed (stampede.toml
    /// `waiters_receive_population_error`).
    pub fn fail_with_error(&self, key: &[u8], error: CacheError) -> Result<(), CacheError> {
        let key_hash = Self::compute_key_hash(key);
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard.find_entry(key_hash).ok_or(CacheError::Miss)?;

        entry.set_state(EntryState::Failed);
        entry.set_last_error(Some(error));
        entry.set_population_owner(false);
        entry.set_population_timestamp(None);
        entry.notify();

        Ok(())
    }

    pub fn release(&self, key: &[u8]) -> Result<(), CacheError> {
        let key_hash = Self::compute_key_hash(key);
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let Some(entry) = guard.find_entry(key_hash) else {
            return Ok(());
        };

        entry.increment_generation();
        entry.set_state(EntryState::Absent);
        entry.set_population_owner(false);
        entry.set_population_timestamp(None);
        entry.set_ttl(None);
        entry.set_last_error(None);
        entry.notify();

        Ok(())
    }

    pub fn invalidate(&self, key: &[u8]) -> Result<(), CacheError> {
        let key_hash = Self::compute_key_hash(key);
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let Some(entry) = guard.find_entry(key_hash) else {
            return Ok(());
        };

        entry.increment_generation();
        entry.set_state(EntryState::Absent);
        entry.set_population_owner(false);
        entry.set_population_timestamp(None);
        entry.set_ttl(None);
        entry.set_last_error(None);
        entry.notify();

        Ok(())
    }

    pub fn health(&self, _key: &[u8]) -> Result<TierHealth, CacheError> {
        Ok(TierHealth::default())
    }

    pub fn set_generation(&self, key: &[u8], generation: Generation) -> Result<(), CacheError> {
        let key_hash = Self::compute_key_hash(key);
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard
            .find_or_create_entry(key_hash)
            .map_err(|_| CacheError::ConfigurationError)?;
        entry.set_generation(generation);
        Ok(())
    }

    pub fn set_tier(&self, key: &[u8], tier: TierId) -> Result<(), CacheError> {
        let key_hash = Self::compute_key_hash(key);
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard
            .find_or_create_entry(key_hash)
            .map_err(|_| CacheError::ConfigurationError)?;
        entry.set_tier(tier);
        Ok(())
    }

    pub fn set_state(&self, key: &[u8], state: EntryState) -> Result<(), CacheError> {
        let key_hash = Self::compute_key_hash(key);
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard
            .find_or_create_entry(key_hash)
            .map_err(|_| CacheError::ConfigurationError)?;
        entry.set_state(state);
        entry.notify();
        Ok(())
    }

    pub fn update_tier_health(&self, _key: &[u8], _health: TierHealth) -> Result<(), CacheError> {
        Ok(())
    }
}

impl Default for Cachelito {
    fn default() -> Self {
        Self::new()
    }
}
