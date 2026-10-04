use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;

use crate::continuity::{RecoveryDirection, RecoveryOutcome};
use crate::entry::{CommitIntent, CommitToken, EntryState, Generation, IntentKind, TierIdLite};
use crate::error::CacheError;
use crate::integrity::{KeyAddress, Placement};
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
    Prepared = 5,
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
            5 => Ok(EntryStateAtomic::Prepared),
            _ => Err(()),
        }
    }
}

impl From<EntryState> for EntryStateAtomic {
    fn from(state: EntryState) -> Self {
        match state {
            EntryState::Absent => Self::Absent,
            EntryState::Ready => Self::Ready,
            EntryState::Stale => Self::Stale,
            EntryState::InFlight => Self::InFlight,
            EntryState::Prepared => Self::Prepared,
            EntryState::Failed => Self::Failed,
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
            EntryStateAtomic::Prepared => EntryState::Prepared,
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
    /// Whether a commit intent is outstanding.
    ///
    /// A separate flag rather than overloading `intent_generation == 0`: an
    /// intent prepared on a never-written key legitimately has generation 0, so
    /// using 0 as the sentinel made exactly those intents invisible — and an
    /// invisible intent is an unrecoverable one.
    intent_present: std::sync::atomic::AtomicBool,
    /// The generation the outstanding intent was prepared for.
    intent_generation: std::sync::atomic::AtomicU64,
    /// `IntentKind::as_u8` while an intent is outstanding, else 0.
    intent_kind: std::sync::atomic::AtomicU8,
    /// Index of the rung the intent targets, else 0.
    intent_tier: std::sync::atomic::AtomicU8,
    /// When the intent was recorded, for the recovery sweep.
    intent_since_nanos: std::sync::atomic::AtomicU64,
    /// The state the entry was in when the intent was recorded.
    ///
    /// `abort` restores this rather than forcing `Failed`. A failed write to a
    /// key that already had a committed value must leave that value readable:
    /// the write was rejected, so nothing about the previously committed state
    /// changed, and forcing `Failed` threw away a good value because a later
    /// write did not land.
    intent_prev_state: std::sync::atomic::AtomicU8,
    /// Identity and placement, held separately. See `KeyAddress`.
    address: KeyAddress,
    notify: Arc<Notify>,
}

impl ControlEntry {
    pub fn new(address: KeyAddress) -> Self {
        ControlEntry {
            state: std::sync::atomic::AtomicU8::new(EntryStateAtomic::Absent as u8),
            generation: std::sync::atomic::AtomicU64::new(0),
            tier: std::sync::atomic::AtomicU8::new(TierId::L0 as u8),
            population_owner: std::sync::atomic::AtomicBool::new(false),
            population_timestamp: std::sync::atomic::AtomicU64::new(0),
            expiration_nanos: std::sync::atomic::AtomicU64::new(0),
            last_error: std::sync::atomic::AtomicU8::new(0),
            intent_present: std::sync::atomic::AtomicBool::new(false),
            intent_generation: std::sync::atomic::AtomicU64::new(0),
            intent_kind: std::sync::atomic::AtomicU8::new(0),
            intent_tier: std::sync::atomic::AtomicU8::new(0),
            intent_since_nanos: std::sync::atomic::AtomicU64::new(0),
            intent_prev_state: std::sync::atomic::AtomicU8::new(EntryStateAtomic::Absent as u8),
            address,
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

    /// Wake every waiter currently registered for this entry.
    ///
    /// `notify_waiters`, not `notify_one`. A single-flight population has *many*
    /// waiters — the 100-caller burst test has 99 — and `notify_one` stores one
    /// permit, so 98 of them waited out the full timeout and the test took five
    /// seconds to report a fetch that had already succeeded.
    ///
    /// `notify_waiters` does not store a permit, so a waiter that has not yet
    /// registered would miss the wakeup. That window is closed on the *waiting*
    /// side rather than here: `Notified::enable()` registers the future before
    /// the waiter decides whether to wait, and the waiter then re-reads the
    /// control plane. So the two halves pair up:
    ///
    /// * not yet registered → the re-read sees the finished state and returns;
    /// * registered → `notify_waiters` reaches it.
    pub fn notify(&self) {
        self.notify.notify_waiters();
    }

    pub fn notify_clone(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    pub fn address(&self) -> KeyAddress {
        self.address
    }

    /// The state an outstanding intent interrupted.
    pub fn intent_prev_state(&self) -> EntryState {
        EntryState::from(
            EntryStateAtomic::try_from(
                self.intent_prev_state
                    .load(std::sync::atomic::Ordering::Acquire),
            )
            .unwrap_or(EntryStateAtomic::Absent),
        )
    }

    pub fn set_intent_prev_state(&self, state: EntryState) {
        self.intent_prev_state.store(
            EntryStateAtomic::from(state) as u8,
            std::sync::atomic::Ordering::Release,
        );
    }

    /// Record a commit intent. Payload-free by construction: a key hash, a
    /// target rung, a generation and a kind. Nothing here can reconstruct a
    /// value, so a crash cannot leak one through the control plane.
    pub fn set_intent(&self, intent: &CommitIntent) {
        self.intent_present
            .store(true, std::sync::atomic::Ordering::Release);
        self.intent_generation
            .store(intent.generation.0, std::sync::atomic::Ordering::Release);
        self.intent_kind
            .store(intent.kind.as_u8(), std::sync::atomic::Ordering::Release);
        self.intent_tier
            .store(intent.target_tier.0, std::sync::atomic::Ordering::Release);
        self.intent_since_nanos
            .store(intent.started_nanos, std::sync::atomic::Ordering::Release);
    }

    pub fn intent(&self) -> Option<CommitIntent> {
        if !self
            .intent_present
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let generation = self
            .intent_generation
            .load(std::sync::atomic::Ordering::Acquire);
        let kind =
            IntentKind::from_u8(self.intent_kind.load(std::sync::atomic::Ordering::Acquire))?;
        Some(CommitIntent {
            kind,
            target_tier: TierIdLite::new(
                self.intent_tier.load(std::sync::atomic::Ordering::Acquire),
            ),
            generation: Generation::new(generation),
            started_nanos: self
                .intent_since_nanos
                .load(std::sync::atomic::Ordering::Acquire),
        })
    }

    pub fn clear_intent(&self) {
        self.intent_present
            .store(false, std::sync::atomic::Ordering::Release);
        self.intent_kind
            .store(0, std::sync::atomic::Ordering::Release);
        self.intent_generation
            .store(0, std::sync::atomic::Ordering::Release);
        self.intent_since_nanos
            .store(0, std::sync::atomic::Ordering::Release);
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
    /// An outstanding commit intent, if any. Present only while the entry is
    /// `Prepared`.
    pub intent: Option<CommitIntent>,
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

    fn find_slot(&self, address: KeyAddress) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let mut idx = (address.placement() as usize) % self.entries.len();
        let mut attempts = 0;
        while attempts < self.entries.len() {
            match &self.entries[idx] {
                Some(entry) if entry.address() == address => return Some(idx),
                None => return None,
                _ => {
                    idx = (idx + 1) % self.entries.len();
                    attempts += 1;
                }
            }
        }
        None
    }

    fn find_empty(&self, address: KeyAddress) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let mut idx = (address.placement() as usize) % self.entries.len();
        let mut attempts = 0;
        while attempts < self.entries.len() {
            match &self.entries[idx] {
                None => return Some(idx),
                Some(entry) if entry.address() == address => return Some(idx),
                _ => {
                    idx = (idx + 1) % self.entries.len();
                    attempts += 1;
                }
            }
        }
        None
    }

    fn find_or_create_entry(
        &mut self,
        address: KeyAddress,
    ) -> Result<&mut ControlEntry, CacheError> {
        if let Some(idx) = self.find_slot(address) {
            return self.entries[idx]
                .as_mut()
                .ok_or(CacheError::ConfigurationError);
        }
        if let Some(idx) = self.find_empty(address) {
            self.entries[idx] = Some(ControlEntry::new(address));
            return self.entries[idx]
                .as_mut()
                .ok_or(CacheError::ConfigurationError);
        }
        Err(CacheError::ConfigurationError)
    }

    fn find_entry(&self, address: KeyAddress) -> Option<&ControlEntry> {
        let idx = self.find_slot(address)?;
        self.entries[idx].as_ref()
    }
}

#[derive(Debug)]
pub struct Cachelito {
    shards: Vec<std::sync::Mutex<Shard>>,
    shard_count: usize,
    _capacity_per_shard: usize,
    /// How keys are mapped to slots. Injectable for the same reason the data
    /// plane's is: without it, a control-plane collision cannot be produced on
    /// demand, so `no_cross_key_corruption` could only be asserted about the
    /// control plane, never demonstrated.
    placement: Placement,
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
            placement: Placement::Default,
        }
    }

    /// The same control plane, addressing keys with a different placement
    /// strategy. Mirrors `FixedTierStub::with_placement`.
    #[must_use]
    pub fn with_placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// Identity and placement for `key`, under the configured strategy.
    pub fn address_of(&self, key: &[u8]) -> KeyAddress {
        KeyAddress::of(key, self.placement)
    }

    /// Shards are chosen by placement, so two keys that collide on it share a
    /// shard and are still told apart there by fingerprint.
    fn shard_for(&self, address: KeyAddress) -> usize {
        (address.placement() as usize) % self.shard_count
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
            intent: entry.intent(),
            notify: entry.notify_clone(),
        }
    }

    pub fn acquire(&self, key: &[u8], tier: TierId) -> Result<ControlSnapshot, CacheError> {
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let Ok(entry) = guard.find_or_create_entry(address) else {
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
                } else if let Err(observed) = result {
                    // The CAS lost: another caller claimed this entry between our
                    // read and our write. Report what the entry actually is *now*,
                    // not the value the CAS returned.
                    //
                    // The CAS's error value is the state it expected to replace,
                    // which is `Absent` — the state we read. Reporting that
                    // produced a snapshot that looked claimable, so the losing
                    // caller went on to become a second population owner and
                    // single-flight admitted two fetches for one key. Re-reading
                    // under the same guard is what makes the loser a waiter.
                    let _ = observed;
                    Ok(Self::snapshot_of(
                        entry,
                        entry.state(),
                        entry.generation(),
                        entry.tier(),
                        entry.is_population_owner(),
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
            // A prepared write owns the entry until it commits or aborts.
            // Reporting it as claimable here would let a second writer prepare
            // on top of an uncommitted one, which is precisely the interleaving
            // the two-phase protocol exists to prevent.
            EntryState::Prepared => Ok(Self::snapshot_of(
                entry,
                EntryState::Prepared,
                current_gen,
                current_tier,
                false,
                None,
            )),
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
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard.find_entry(address).ok_or(CacheError::Miss)?;

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
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard.find_entry(address).ok_or(CacheError::Miss)?;

        entry.set_state(EntryState::Failed);
        entry.set_last_error(Some(error));
        entry.set_population_owner(false);
        entry.set_population_timestamp(None);
        entry.notify();

        Ok(())
    }

    pub fn release(&self, key: &[u8]) -> Result<(), CacheError> {
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let Some(entry) = guard.find_entry(address) else {
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
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let Some(entry) = guard.find_entry(address) else {
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
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard
            .find_or_create_entry(address)
            .map_err(|_| CacheError::ConfigurationError)?;
        entry.set_generation(generation);
        Ok(())
    }

    pub fn set_tier(&self, key: &[u8], tier: TierId) -> Result<(), CacheError> {
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard
            .find_or_create_entry(address)
            .map_err(|_| CacheError::ConfigurationError)?;
        entry.set_tier(tier);
        Ok(())
    }

    pub fn set_state(&self, key: &[u8], state: EntryState) -> Result<(), CacheError> {
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let entry = guard
            .find_or_create_entry(address)
            .map_err(|_| CacheError::ConfigurationError)?;
        entry.set_state(state);
        entry.notify();
        Ok(())
    }

    pub fn update_tier_health(&self, _key: &[u8], _health: TierHealth) -> Result<(), CacheError> {
        Ok(())
    }

    /// Observe an entry without claiming it.
    ///
    /// `acquire` mutates: it creates the slot, transitions `Absent -> InFlight`,
    /// bumps the generation and takes population ownership. That is right for an
    /// operation that may populate and wrong for a read-only probe — which is
    /// exactly what `exists` used to do, leaving every key it was asked about
    /// wedged in `InFlight` with an owner that would never publish.
    ///
    /// `peek` answers the same question with no side effect, and does not create
    /// a slot for a key that has never been written.
    pub fn peek(&self, key: &[u8]) -> Result<ControlSnapshot, CacheError> {
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        #[allow(clippy::significant_drop_tightening)]
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;

        let Some(entry) = guard.find_entry(address) else {
            // Never written. Report a synthetic Absent so callers need no Option
            // branch, and do not allocate a slot for it.
            return Ok(ControlSnapshot {
                state: EntryState::Absent,
                generation: Generation::new(0),
                tier: TierId::L0,
                population_owner: false,
                population_timestamp: None,
                ttl: None,
                expired: false,
                last_error: None,
                intent: None,
                notify: Arc::new(Notify::new()),
            });
        };

        Ok(Self::snapshot_of(
            entry,
            entry.state(),
            entry.generation(),
            entry.tier(),
            entry.is_population_owner(),
            entry.population_timestamp(),
        ))
    }

    /// Advance the generation by exactly one, under the shard lock.
    ///
    /// The previous manager-side pattern was `set_generation(snapshot.gen + 1)`
    /// where `snapshot` had been read *before* an `.await`. An invalidation
    /// landing in that window bumped the generation, and the store then wrote
    /// back the pre-await value — silently undoing the invalidation. Doing the
    /// read-modify-write under the guard makes it indivisible, so a concurrent
    /// bump is composed with rather than overwritten.
    pub fn bump_generation(&self, key: &[u8]) -> Result<Generation, CacheError> {
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;
        let entry = guard
            .find_or_create_entry(address)
            .map_err(|_| CacheError::ConfigurationError)?;
        let next = entry.increment_generation();
        entry.notify();
        Ok(next)
    }

    /// Reserve `address` for eviction, or refuse.
    ///
    /// The authoritative half of eviction (B17). The tier nominates a victim;
    /// this decides whether that victim is *admissible*, and it decides it
    /// under the shard lock so the check and the invalidation are indivisible.
    ///
    /// Admissible means: the entry exists, is `Ready`, is not held by a
    /// population, and has no outstanding commit intent. On success the
    /// generation is advanced **before** returning, which is what makes the
    /// eviction safe: any in-flight `commit` holding a token for the old
    /// generation finds `token.expired_by(entry.generation())` true and is
    /// refused, so it cannot land a write into a slot that is about to be
    /// reused. The entry is moved to `Stale` so a concurrent `get` reports a
    /// miss rather than the value being removed.
    ///
    /// Synchronous, and it takes and releases the guard before returning. That
    /// is not incidental: holding a shard guard across the manager's subsequent
    /// `remove_if_address().await` is precisely the "no guard across `.await`"
    /// rule this control plane is built to make structural.
    ///
    /// Refusing is a normal outcome, not an error: an entry that has become
    /// `InFlight` since nomination is simply not evictable yet, and the caller
    /// moves on to another candidate.
    pub fn reserve_eviction(
        &self,
        address: crate::integrity::KeyAddress,
    ) -> Result<bool, CacheError> {
        let shard_idx = self.shard_for_address(address);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;
        let Some(entry) = guard.find_entry(address) else {
            return Ok(false);
        };

        // Admissibility. All four are read under the same guard that performs
        // the invalidation below, so none can change in between.
        if entry.state() != EntryState::Ready || entry.is_population_owner() {
            return Ok(false);
        }
        // An outstanding intent means a writer is between `prepare` and
        // `commit`. Evicting now would either strand its write or lose it
        // silently; neither is this method's decision to make.
        if entry.intent().is_some() {
            return Ok(false);
        }

        entry.increment_generation();
        entry.set_state(EntryState::Stale);
        entry.notify();
        Ok(true)
    }

    /// Record a commit intent and move the entry to `Prepared`.
    ///
    /// The write half of two-phase commit. After it returns the entry is
    /// `Prepared`: reads see a miss, another writer is refused rather than
    /// allowed to interleave, and a crash leaves a payload-free record that
    /// recovery can resolve.
    /// `expected_generation` is `Some` when the caller read the entry and wants
    /// the prepare to fail if it has moved since, and `None` when the caller is
    /// starting an operation from scratch. It is an `Option` rather than
    /// "generation 0 means any" because 0 is a real generation — the one a
    /// never-written key has — and overloading it makes "expect generation 0"
    /// silently mean "expect nothing".
    pub fn prepare(
        &self,
        key: &[u8],
        expected_generation: Option<Generation>,
        target_tier: TierId,
        kind: IntentKind,
    ) -> Result<CommitToken, CacheError> {
        let address = self.address_of(key);
        let shard_idx = self.shard_for(address);
        let shard = &self.shards[shard_idx];
        let mut guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;
        let entry = guard
            .find_or_create_entry(address)
            .map_err(|_| CacheError::ConfigurationError)?;

        let current = entry.generation();
        // The entry must not have moved since the caller read it, or the data
        // step is about to write a value derived from a superseded decision.
        if let Some(expected) = expected_generation
            && expected.0 != current.0
        {
            return Err(CacheError::StaleGeneration);
        }

        let intent = CommitIntent {
            kind,
            target_tier: TierIdLite::new(target_tier.as_u8()),
            generation: current,
            started_nanos: Self::now_nanos(),
        };
        entry.set_intent_prev_state(entry.state());
        entry.set_intent(&intent);
        entry.set_state(EntryState::Prepared);
        entry.set_population_owner(true);

        Ok(CommitToken {
            address,
            generation: current,
            kind,
            target_tier: intent.target_tier,
        })
    }

    /// Commit a prepared intent: the value becomes visible.
    ///
    /// Verifies the generation is still the one the intent was prepared for, and
    /// that the entry is still `Prepared`. Two racing writers can both prepare,
    /// but only one commits; the loser gets `StaleGeneration` and cannot make its
    /// value visible. Committing an entry recovery already aborted is rejected
    /// rather than resurrecting a resolved intent.
    pub fn commit(
        &self,
        token: &CommitToken,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        let shard_idx = self.shard_for_address(token.address);
        let shard = &self.shards[shard_idx];
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;
        let Some(entry) = guard.find_entry(token.address) else {
            return Err(CacheError::Miss);
        };

        if token.expired_by(entry.generation()) || entry.state() != EntryState::Prepared {
            return Err(CacheError::StaleGeneration);
        }

        entry.set_state(EntryState::Ready);
        if let Some(tier) = TierId::from_index(token.target_tier.0) {
            entry.set_tier(tier);
        }
        entry.set_ttl(ttl);
        entry.set_intent_prev_state(EntryState::Ready);
        entry.set_population_owner(false);
        entry.clear_intent();
        // Re-arm so TTL is measured from commit time, not from prepare time.
        entry.set_population_timestamp(Some(Instant::now()));
        entry.set_last_error(None);
        entry.notify();
        Ok(())
    }

    /// Abandon a prepared intent whose data step **may** have reached the tier.
    ///
    /// This is the safe default, and it is conservative on purpose: the control
    /// plane has no tier handle, so it cannot know whether a write landed. Only
    /// the backend knows, and `CacheError` has a `WriteIndeterminate` variant
    /// precisely because that cannot be inferred from the error — both
    /// `TierUnavailable` and `Timeout` are compatible with a write that landed.
    /// So this assumes residue exists, and makes it unreachable.
    ///
    /// It used to restore `intent_prev_state` unconditionally, which is where
    /// `partial_commit_visible = false` broke. The justification was that the
    /// entry reads as `Prepared` during the data step, so the read path never
    /// reaches the rung and residue is unreachable. That holds *while the entry
    /// stays `Prepared`* — and restoring `Ready` is exactly what stops it
    /// staying `Prepared`. On a key that was already `Ready`:
    ///
    /// ```text
    /// Ready("old") -> Prepared -> tier.set("new") -> cancel -> abort -> Ready
    /// ```
    ///
    /// the rung now holds `"new"` and the control plane says `Ready`, so the next
    /// read serves a value nobody authorised. The same window existed on the
    /// recovery sweep, which has no tier handle at all and so cannot remove
    /// residue even in principle.
    ///
    /// `Failed` is not served by the read path, so the entry misses and the next
    /// read repopulates. That is a cache missing once, not data loss: the rung
    /// still holds whatever is there, and repopulation reads it back.
    ///
    /// Idempotent — aborting an already-aborted entry succeeds — which is what
    /// makes repeated recovery safe.
    pub fn abort(&self, token: &CommitToken, error: CacheError) -> Result<(), CacheError> {
        self.abort_inner(token, error, false)
    }

    /// Abandon an intent the tier **provably** never wrote.
    ///
    /// Restores the interrupted state, so a previously-committed value stays
    /// readable. Only for errors that establish the write never reached the
    /// rung: `SerializationFailed` (the value never encoded) and
    /// `CapacityExhausted` (admission refused it). Every other error is
    /// compatible with a landed write — including the ones the manager infers
    /// rather than the tier reports — so it gets [`Self::abort`].
    ///
    /// Split out rather than given a `bool` argument because the caller, not this
    /// function, is what knows which errors carry that guarantee. A boolean
    /// eventually gets passed `true` by someone who had not proved it.
    pub fn abort_proven_clean(
        &self,
        token: &CommitToken,
        error: CacheError,
    ) -> Result<(), CacheError> {
        self.abort_inner(token, error, true)
    }

    fn abort_inner(
        &self,
        token: &CommitToken,
        error: CacheError,
        proven_clean: bool,
    ) -> Result<(), CacheError> {
        let shard_idx = self.shard_for_address(token.address);
        let shard = &self.shards[shard_idx];
        #[allow(clippy::significant_drop_tightening)]
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;
        let Some(entry) = guard.find_entry(token.address) else {
            // Already gone. Recovery is idempotent, so this is a success.
            return Ok(());
        };

        // A commit that already won must not be undone.
        //
        // `abort` and `commit` can interleave: the committer's CAS from
        // `Prepared` can land microseconds before this runs. Restoring the
        // pre-intent state then would overwrite `Ready` with `Absent` and make a
        // successfully committed value invisible. A loom model of exactly this
        // interleaving found it.
        //
        // So: only touch an entry that is still `Prepared`. Anything else has
        // already moved on, and the correct action for a stale abort is none.
        if entry.state() != EntryState::Prepared {
            return Ok(());
        }

        if proven_clean {
            // Restore the interrupted state rather than forcing `Failed`.
            //
            // The tier provably stored nothing, so nothing about the previously
            // committed value changed: the write was rejected before it could
            // replace anything. Forcing `Failed` made a rejected write to a
            // populated key render that key unreadable, which is data loss caused
            // by an error that should have been a no-op.
            //
            // The generation is deliberately *not* advanced. It is what makes a
            // second `commit` with the same token fail, but `commit` also requires
            // the entry to still be `Prepared`, and this abort has just made it not
            // `Prepared`. So the double-committed token is still rejected, without
            // invalidating a good value.
            let restored = entry.intent_prev_state();
            entry.set_state(restored);
        } else {
            // The write may have landed, so `Failed` is the only state the read
            // path will not serve, and therefore the only one that cannot expose
            // residue. The generation advances so a value stored under this intent
            // cannot later be mistaken for one stored under a subsequent prepare.
            // That is what this function's old doc comment claimed it did; it did
            // not, and nothing else invalidated what landed.
            entry.set_state(EntryState::Failed);
            entry.increment_generation();
        }
        entry.set_population_owner(false);
        entry.clear_intent();
        entry.set_last_error(Some(error));
        entry.notify();
        Ok(())
    }

    /// Every outstanding intent older than `older_than_nanos`.
    ///
    /// The recovery sweep. Returns key hashes because the control plane only ever
    /// stores hashes; resolving an intent needs the key, which only the owning
    /// `CacheManager` still has.
    pub fn stale_intents(&self, older_than_nanos: u64) -> Vec<(KeyAddress, CommitIntent)> {
        let cutoff = Self::now_nanos().saturating_sub(older_than_nanos);
        let mut out = Vec::new();
        for shard in &self.shards {
            let Ok(guard) = shard.lock() else { continue };
            for slot in guard.entries.iter().flatten() {
                let Some(intent) = slot.intent() else {
                    continue;
                };
                if intent.started_nanos <= cutoff {
                    out.push((slot.address(), intent));
                }
            }
        }
        out
    }

    /// Abort an intent by address, for a recovery sweep that has no key bytes.
    ///
    /// The sweep needs no key: the address carries the fingerprint that decides
    /// identity and the placement that picks the shard, so the entry is found
    /// without ever reconstructing the bytes.
    ///
    /// The earlier version of this took a key slice and the sweep passed an empty
    /// one, which hashed to the wrong slot and aborted nothing. That failure was
    /// silent: recovery reported success and left the intent outstanding.
    pub fn abort_intent_by_address(
        &self,
        address: KeyAddress,
        error: CacheError,
    ) -> Result<bool, CacheError> {
        let shard_idx = self.shard_for_address(address);
        let shard = &self.shards[shard_idx];
        #[allow(clippy::significant_drop_tightening)]
        let guard = shard.lock().map_err(|_| CacheError::ConfigurationError)?;
        let Some(entry) = guard.find_entry(address) else {
            return Ok(false);
        };
        // Report whether an intent was actually cleared.
        //
        // Aborting stays idempotent — a second sweep must be a no-op, not an
        // error — but returning `Ok(())` unconditionally made two concurrent
        // sweeps both count the same intent, so a recovery pass reported 28
        // recoveries for 16 intents. A count that overstates what happened is
        // worse than no count: it makes the sweep look effective when it may not
        // have been.
        if entry.intent().is_none() {
            return Ok(false);
        }
        // Same rule as `abort`: never overwrite a commit that already landed.
        if entry.state() != EntryState::Prepared {
            return Ok(false);
        }
        // Conservative, like `abort`: a sweep has no tier handle, so it cannot
        // know whether the data step landed and cannot remove residue even in
        // principle. Restoring the interrupted state here is what made a swept
        // write's residue readable — the entry went back to `Ready` over a rung
        // nobody had cleaned. `Failed` is not served, so the next read repopulates.
        entry.set_state(EntryState::Failed);
        entry.increment_generation();
        entry.set_population_owner(false);
        entry.clear_intent();
        entry.set_last_error(Some(error));
        entry.notify();
        Ok(true)
    }

    /// Resolve an intent discovered by [`Self::stale_intents`], given the key.
    ///
    /// Returns what recovery did so the outcome is reportable.
    /// `recovery_failure_must_be_observable` is a contract clause, and a
    /// recovery pass that reports nothing is how a silently-diverged entry
    /// survives a restart.
    pub fn resolve_intent(
        &self,
        key: &[u8],
        intent: CommitIntent,
        direction: RecoveryDirection,
    ) -> Result<RecoveryOutcome, CacheError> {
        let token = CommitToken {
            address: self.address_of(key),
            generation: intent.generation,
            kind: intent.kind,
            target_tier: intent.target_tier,
        };
        match direction {
            RecoveryDirection::Abort => {
                self.abort(&token, CacheError::UncommittedIntent)?;
                Ok(RecoveryOutcome::Aborted { kind: intent.kind })
            }
            RecoveryDirection::CompleteForward => {
                self.commit(&token, None)?;
                Ok(RecoveryOutcome::Completed { kind: intent.kind })
            }
            // Declines rather than guessing. Completing a move forward needs the
            // source rung read and the destination written, and this call has the
            // key but no tier handles — so a caller wanting the move finished
            // must drive `CacheManager`, which has both. Reporting failure here is
            // the honest answer; silently committing the control-plane half would
            // publish a move whose data half never happened.
            RecoveryDirection::ExternalReconciliation => Err(CacheError::ConfigurationError),
        }
    }

    fn shard_for_address(&self, address: KeyAddress) -> usize {
        self.shard_for(address)
    }

    fn now_nanos() -> u64 {
        ControlEntry::instant_to_nanos(Instant::now())
    }
}

impl Default for Cachelito {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::Placement;

    /// Anti-vacuity for the control-plane collision test: if these did not
    /// collide on placement, the test using them would pass without ever
    /// exercising the probe-conflict path.
    #[test]
    fn placement_strategies_really_collide_for_the_control_plane() {
        assert_eq!(
            Placement::AllToZero.hash(b"alpha"),
            Placement::AllToZero.hash(b"beta")
        );
        let p = Placement::CollidingPair { prefix: b"k" };
        assert_eq!(p.hash(b"k1"), p.hash(b"k2"));
        assert_ne!(p.hash(b"k1"), p.hash(b"other"));
    }

    /// Two distinct keys forced onto one placement hash must hold independent
    /// state.
    ///
    /// This is the finding that could not be tested before. `ControlEntry` stored
    /// a bare `u64` that was simultaneously the slot selector and the identity,
    /// so `find_slot` decided "same key" by comparing the number that chose the
    /// slot — and `Cachelito` had no way to make two keys collide on it. The
    /// property was asserted for the control plane and undemonstrable there.
    ///
    /// With `Placement::AllToZero` both keys start at slot 0 and walk the same
    /// probe sequence, which is exactly the shared-slot case. Before the split
    /// they resolved to one entry: one generation, one owner, one intent, and one
    /// rung applied to both keys.
    #[test]
    fn a_placement_collision_does_not_alias_two_control_entries() {
        let cachelito = Cachelito::new().with_placement(Placement::AllToZero);

        // Independent state for each key, so an aliasing entry would be visible.
        let a = cachelito
            .prepare(b"alpha", None, TierId::L1, IntentKind::Write)
            .expect("prepare alpha");
        let b = cachelito
            .prepare(b"beta", None, TierId::L2, IntentKind::Write)
            .expect("prepare beta");

        assert_ne!(
            a.address.fingerprint(),
            b.address.fingerprint(),
            "the test premise is broken: the two keys have one fingerprint"
        );
        assert_eq!(
            a.address.placement(),
            b.address.placement(),
            "the test premise is broken: the two keys did not collide on placement"
        );

        // Commit alpha, leave beta prepared. An aliased entry would make these
        // two observations the same entry.
        cachelito.commit(&a, None).expect("commit alpha");
        let alpha = cachelito.peek(b"alpha").expect("peek alpha");
        let beta = cachelito.peek(b"beta").expect("peek beta");
        assert_eq!(alpha.state, EntryState::Ready);
        assert_eq!(
            beta.state,
            EntryState::Prepared,
            "committing one key resolved the other's entry: they share a slot"
        );

        // Committing beta is legitimate — it has its own prepared intent — and the
        // rung each entry lands on is the decisive evidence they stayed separate.
        // Both were prepared for *different* tiers, so an aliased entry would have
        // collapsed them onto one rung.
        cachelito.commit(&b, None).expect("commit beta");
        let alpha = cachelito.peek(b"alpha").expect("peek alpha");
        let beta = cachelito.peek(b"beta").expect("peek beta");
        assert_eq!(alpha.tier, TierId::L1, "alpha's rung moved");
        assert_eq!(
            beta.tier,
            TierId::L2,
            "beta took alpha's rung: the two keys share one control entry"
        );
        assert_eq!(alpha.generation, beta.generation);

        // Aborting one key must not disturb the other's committed value.
        cachelito
            .abort(&b, CacheError::Cancelled)
            .expect("abort beta");
        assert_eq!(
            cachelito.peek(b"alpha").expect("peek alpha").state,
            EntryState::Ready,
            "aborting one key disturbed the other's committed value"
        );
    }

    /// A forced 64-bit placement collision must not disturb identity, and the
    /// specific collision pair must be the one the strategy promises.
    #[test]
    fn a_chosen_collision_pair_stays_distinct() {
        let cachelito = Cachelito::new().with_placement(Placement::CollidingPair { prefix: b"k" });
        let one = cachelito.address_of(b"k1");
        let two = cachelito.address_of(b"k2");
        assert_eq!(one.placement(), two.placement());
        assert_ne!(one.fingerprint(), two.fingerprint());

        cachelito
            .prepare(b"k1", None, TierId::L1, IntentKind::Write)
            .expect("prepare k1");
        cachelito
            .prepare(b"k2", None, TierId::L1, IntentKind::Write)
            .expect("prepare k2");

        // Both keys must exist independently: two entries, not one.
        let mut intents = cachelito.stale_intents(0);
        assert_eq!(
            intents.len(),
            2,
            "two colliding keys resolved to a single control entry"
        );
        intents.sort_by_key(|(a, _)| a.fingerprint());
        assert_ne!(intents[0].0.fingerprint(), intents[1].0.fingerprint());
    }
}
