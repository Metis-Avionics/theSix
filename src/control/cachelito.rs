use std::hash::Hasher;
use std::sync::Arc;
use std::time::Instant;

use crate::entry::{EntryState, Generation};
use crate::error::CacheError;
use crate::tier::tier_trait::TierHealth;
use crate::tier::TierId;

const DEFAULT_SHARD_COUNT: usize = 16;

#[derive(Debug, Clone)]
pub struct ControlEntry {
    pub state: EntryState,
    pub generation: Generation,
    pub tier: TierId,
    pub population_owner: bool,
    pub population_timestamp: Option<Instant>,
    pub tier_health: TierHealth,
    pub notify: Arc<tokio::sync::Notify>,
}

impl ControlEntry {
    pub fn new(tier: TierId) -> Self {
        ControlEntry {
            state: EntryState::Absent,
            generation: Generation::new(0),
            tier,
            population_owner: false,
            population_timestamp: None,
            tier_health: TierHealth::default(),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

pub struct ControlSnapshot {
    pub key: Vec<u8>,
    pub state: EntryState,
    pub generation: Generation,
    pub tier: TierId,
    pub population_owner: bool,
    pub population_timestamp: Option<Instant>,
    pub tier_health: TierHealth,
    pub notify: Arc<tokio::sync::Notify>,
}

#[derive(Debug, Clone)]
pub struct Cachelito {
    shards: Arc<Vec<dashmap::DashMap<Vec<u8>, ControlEntry>>>,
    shard_count: usize,
}

impl Cachelito {
    pub fn new() -> Self {
        Self::with_shards(DEFAULT_SHARD_COUNT)
    }

    pub fn with_shards(shard_count: usize) -> Self {
        let mut shards = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            shards.push(dashmap::DashMap::new());
        }
        Cachelito {
            shards: Arc::new(shards),
            shard_count,
        }
    }

    fn shard_for(&self, key: &[u8]) -> usize {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(key, &mut hasher);
        (hasher.finish() as usize) % self.shard_count
    }

    pub fn acquire(&self, key: &[u8], tier: TierId) -> ControlSnapshot {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let entry = shard
            .entry(key.to_vec())
            .or_insert_with(|| ControlEntry::new(tier));

        ControlSnapshot {
            key: key.to_vec(),
            state: entry.state,
            generation: entry.generation,
            tier: entry.tier,
            population_owner: entry.population_owner,
            population_timestamp: entry.population_timestamp,
            tier_health: entry.tier_health.clone(),
            notify: Arc::clone(&entry.notify),
        }
    }

    pub fn publish(
        &self,
        key: &[u8],
        generation: Generation,
        tier: TierId,
    ) -> Result<(), CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let mut entry = shard.get_mut(key).ok_or(CacheError::Miss)?;

        if generation.is_stale(entry.generation) {
            return Err(CacheError::StaleGeneration);
        }

        entry.state = EntryState::Ready;
        entry.generation = generation;
        entry.tier = tier;
        entry.population_owner = false;
        entry.population_timestamp = None;

        entry.notify.notify_waiters();

        Ok(())
    }

    pub fn fail(&self, key: &[u8]) -> Result<(), CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let mut entry = shard.get_mut(key).ok_or(CacheError::Miss)?;

        entry.state = EntryState::Failed;
        entry.population_owner = false;
        entry.population_timestamp = None;

        entry.tier_health.consecutive_failures += 1;
        entry.tier_health.last_failure_timestamp = Some(std::time::SystemTime::now());
        entry.tier_health.health_score = (entry.tier_health.health_score - 0.1).max(0.0);

        entry.notify.notify_waiters();

        Ok(())
    }

    pub fn release(&self, key: &[u8]) -> Result<(), CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let mut entry = shard.get_mut(key).ok_or(CacheError::Miss)?;

        entry.state = EntryState::Absent;
        entry.population_owner = false;
        entry.population_timestamp = None;

        entry.notify.notify_waiters();

        Ok(())
    }

    pub fn health(&self, key: &[u8]) -> Result<TierHealth, CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let entry = shard.get(key).ok_or(CacheError::Miss)?;

        Ok(entry.tier_health.clone())
    }

    pub fn set_state(&self, key: &[u8], state: EntryState) -> Result<(), CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let mut entry = shard.get_mut(key).ok_or(CacheError::Miss)?;
        entry.state = state;
        entry.notify.notify_waiters();

        Ok(())
    }

    pub fn set_generation(&self, key: &[u8], generation: Generation) -> Result<(), CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let mut entry = shard.get_mut(key).ok_or(CacheError::Miss)?;
        entry.generation = generation;
        entry.notify.notify_waiters();

        Ok(())
    }

    pub fn set_tier(&self, key: &[u8], tier: TierId) -> Result<(), CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let mut entry = shard.get_mut(key).ok_or(CacheError::Miss)?;
        entry.tier = tier;

        Ok(())
    }

    pub fn mark_population_start(
        &self,
        key: &[u8],
        generation: Generation,
    ) -> Result<(), CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let mut entry = shard
            .entry(key.to_vec())
            .or_insert_with(|| ControlEntry::new(TierId::L0));
        entry.population_owner = true;
        entry.population_timestamp = Some(Instant::now());
        entry.state = EntryState::InFlight;
        entry.generation = generation;
        entry.notify.notify_waiters();

        Ok(())
    }

    pub fn update_tier_health(&self, key: &[u8], health: TierHealth) -> Result<(), CacheError> {
        let shard_idx = self.shard_for(key);
        let shard = &self.shards[shard_idx];

        let mut entry = shard.get_mut(key).ok_or(CacheError::Miss)?;
        entry.tier_health = health;

        Ok(())
    }
}

impl Default for Cachelito {
    fn default() -> Self {
        Self::new()
    }
}
