use std::sync::Arc;
use std::time::Duration;

use crate::entry::{EntryState, Generation};
use crate::error::CacheError;
use crate::identity::IdentityContext;
use crate::policy::{CacheOperation, CachePolicy, CacheRequest, CacheState, FailMode};
use crate::tier::tier_trait::CacheTier;
use crate::tier::TierId;
use crate::tier::TierRegistry;

pub struct CacheManager<K, V, P> {
    policy: P,
    cachelito: crate::control::cachelito::Cachelito,
    tier_registry: TierRegistry,
    tiers: Vec<Arc<dyn CacheTier<V>>>,
    wait_timeout: Duration,
    _key: std::marker::PhantomData<K>,
    _value: std::marker::PhantomData<V>,
}

impl<K, V, P> CacheManager<K, V, P>
where
    K: std::hash::Hash + Eq + Clone + Send + Sync + std::fmt::Debug,
    V: Clone + Send + Sync + 'static,
    P: CachePolicy<K, V>,
{
    pub fn new(
        policy: P,
        cachelito: crate::control::cachelito::Cachelito,
        tier_registry: TierRegistry,
        tiers: Vec<Arc<dyn CacheTier<V>>>,
    ) -> Self {
        Self::with_timeout(
            policy,
            cachelito,
            tier_registry,
            tiers,
            Duration::from_secs(5),
        )
    }

    pub fn with_timeout(
        policy: P,
        cachelito: crate::control::cachelito::Cachelito,
        tier_registry: TierRegistry,
        tiers: Vec<Arc<dyn CacheTier<V>>>,
        wait_timeout: Duration,
    ) -> Self {
        CacheManager {
            policy,
            cachelito,
            tier_registry,
            tiers,
            wait_timeout,
            _key: std::marker::PhantomData,
            _value: std::marker::PhantomData,
        }
    }

    pub fn cachelito(&self) -> &crate::control::cachelito::Cachelito {
        &self.cachelito
    }

    pub async fn get(&self, key: &K) -> Result<Option<V>, CacheError> {
        self.check_auth()?;

        let key_bytes = self.key_to_bytes(key);
        let request = CacheRequest::new(CacheOperation::Get, key.clone());
        let decision =
            self.policy
                .select(&request, &CacheState::new(), &IdentityContext::anonymous());

        if !decision.authorized {
            return Err(CacheError::Unauthorized);
        }

        let snapshot = self.cachelito.acquire(&key_bytes, TierId::L0);

        if snapshot.state == EntryState::Ready {
            let tier = self.tier_for(&snapshot.tier);
            return tier.get(&key_bytes);
        }

        if snapshot.state == EntryState::InFlight {
            self.wait_for_population(&snapshot).await?;
            let tier = self.tier_for(&snapshot.tier);
            return tier.get(&key_bytes);
        }

        Err(CacheError::Miss)
    }

    pub async fn get_or_fetch<F, Fut>(&self, key: &K, fetch: F) -> Result<V, CacheError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, CacheError>>,
    {
        self.check_auth()?;

        let key_bytes = self.key_to_bytes(key);
        let request = CacheRequest::new(CacheOperation::Get, key.clone());

        let snapshot = self.cachelito.acquire(&key_bytes, TierId::L0);

        if snapshot.state == EntryState::Ready {
            let tier = self.tier_for(&snapshot.tier);
            if let Some(v) = tier.get(&key_bytes)? {
                return Ok(v);
            }
        }

        if snapshot.state == EntryState::InFlight {
            return self.wait_and_get(key, &key_bytes, &snapshot).await;
        }

        self.become_population_owner(key, &key_bytes, &request, fetch)
            .await
    }

    pub async fn set(&self, key: K, value: V) -> Result<(), CacheError> {
        self.check_auth()?;

        let key_bytes = self.key_to_bytes(&key);
        let request = CacheRequest::new(CacheOperation::Set, key);

        let snapshot = self.cachelito.acquire(&key_bytes, TierId::L0);
        let decision =
            self.policy
                .select(&request, &CacheState::new(), &IdentityContext::anonymous());

        if !decision.authorized {
            return Err(CacheError::Unauthorized);
        }

        let tier = self.tier_for(&decision.tier);
        tier.set(&key_bytes, value, None)?;

        let generation = Generation::new(snapshot.generation.0 + 1);
        self.cachelito.set_generation(&key_bytes, generation)?;
        self.cachelito
            .publish(&key_bytes, generation, decision.tier)?;

        Ok(())
    }

    pub async fn invalidate(&self, key: &K) -> Result<(), CacheError> {
        self.check_auth()?;

        let key_bytes = self.key_to_bytes(key);
        self.cachelito.release(&key_bytes)?;

        Ok(())
    }

    pub async fn remove(&self, key: &K) -> Result<(), CacheError> {
        self.check_auth()?;

        let key_bytes = self.key_to_bytes(key);

        for tier in &self.tiers {
            let _ = tier.remove(&key_bytes);
        }

        self.cachelito.release(&key_bytes)?;

        Ok(())
    }

    pub async fn promote(&self, key: &K) -> Result<(), CacheError> {
        self.check_auth()?;
        let key_bytes = self.key_to_bytes(key);

        let snapshot = self.cachelito.acquire(&key_bytes, TierId::L0);
        if snapshot.state != EntryState::Ready {
            return Err(CacheError::Miss);
        }

        let current_idx = snapshot.tier.as_usize();
        if current_idx == 0 {
            return Ok(());
        }

        let from_tier = self.tier_for(&snapshot.tier);
        let value = from_tier.get(&key_bytes)?.ok_or(CacheError::Miss)?;

        let new_tier_id = TierId::from_usize(current_idx - 1).unwrap();
        let to_tier = self.tier_for(&new_tier_id);
        to_tier.set(&key_bytes, value, None)?;

        self.cachelito.set_tier(&key_bytes, new_tier_id)?;

        Ok(())
    }

    pub async fn demote(&self, key: &K) -> Result<(), CacheError> {
        self.check_auth()?;
        let key_bytes = self.key_to_bytes(key);

        let snapshot = self.cachelito.acquire(&key_bytes, TierId::L0);
        if snapshot.state != EntryState::Ready {
            return Err(CacheError::Miss);
        }

        let current_idx = snapshot.tier.as_usize();
        if current_idx == 5 {
            return Ok(());
        }

        let from_tier = self.tier_for(&snapshot.tier);
        let value = from_tier.get(&key_bytes)?.ok_or(CacheError::Miss)?;

        let new_tier_id = TierId::from_usize(current_idx + 1).unwrap();
        let to_tier = self.tier_for(&new_tier_id);
        to_tier.set(&key_bytes, value, None)?;

        self.cachelito.set_tier(&key_bytes, new_tier_id)?;

        Ok(())
    }

    fn check_auth(&self) -> Result<(), CacheError> {
        Ok(())
    }

    fn key_to_bytes(&self, key: &K) -> Vec<u8> {
        format!("{:?}", key).into_bytes()
    }

    pub fn tier_for(&self, tier_id: &TierId) -> Arc<dyn CacheTier<V>> {
        let idx = tier_id.as_usize();
        self.tiers[idx].clone()
    }

    pub fn tier(&self, tier_id: &TierId) -> Arc<dyn CacheTier<V>> {
        self.tier_for(tier_id)
    }

    async fn wait_for_population(
        &self,
        snapshot: &crate::control::cachelito::ControlSnapshot,
    ) -> Result<(), CacheError> {
        tokio::select! {
            _ = tokio::time::sleep(self.wait_timeout) => {
                Err(CacheError::Timeout)
            }
            _ = snapshot.notify.notified() => {
                Ok(())
            }
        }
    }

    async fn wait_and_get(
        &self,
        _key: &K,
        key_bytes: &[u8],
        snapshot: &crate::control::cachelito::ControlSnapshot,
    ) -> Result<V, CacheError> {
        self.wait_for_population(snapshot).await?;

        let tier = self.tier_for(&snapshot.tier);
        match tier.get(key_bytes)? {
            Some(v) => Ok(v),
            None => Err(CacheError::Miss),
        }
    }

    async fn become_population_owner<F, Fut>(
        &self,
        _key: &K,
        key_bytes: &[u8],
        request: &CacheRequest<K, V>,
        fetch: F,
    ) -> Result<V, CacheError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, CacheError>>,
    {
        let generation = Generation::new(1);

        self.cachelito
            .mark_population_start(key_bytes, generation)?;

        let result = tokio::time::timeout(self.wait_timeout, fetch()).await;

        match result {
            Ok(Ok(value)) => {
                let decision =
                    self.policy
                        .select(request, &CacheState::new(), &IdentityContext::anonymous());

                let tier_id = decision.tier;
                let tier = self.tier_for(&tier_id);
                tier.set(key_bytes, value, None)?;

                match self.try_pubulate(key_bytes, generation, tier_id).await {
                    Ok(()) => match tier.get(key_bytes)? {
                        Some(v) => Ok(v),
                        None => Err(CacheError::Miss),
                    },
                    Err(CacheError::StaleGeneration) => Err(CacheError::StaleGeneration),
                    Err(e) => {
                        let _ = self.cachelito.fail(key_bytes);
                        if decision.fail_mode == FailMode::Open {
                            self.try_fallback_tier(key_bytes, request).await
                        } else {
                            Err(e)
                        }
                    }
                }
            }
            Err(_) => {
                let _ = self.cachelito.fail(key_bytes);
                Err(CacheError::Timeout)
            }
            Ok(Err(e)) => {
                let _ = self.cachelito.fail(key_bytes);
                Err(e)
            }
        }
    }

    async fn try_pubulate(
        &self,
        key_bytes: &[u8],
        generation: Generation,
        tier_id: TierId,
    ) -> Result<(), CacheError> {
        self.cachelito.publish(key_bytes, generation, tier_id)
    }

    async fn try_fallback_tier(
        &self,
        key_bytes: &[u8],
        _request: &CacheRequest<K, V>,
    ) -> Result<V, CacheError> {
        for tier_id in self.tier_registry.all() {
            if *tier_id == TierId::L0 {
                continue;
            }
            if let Ok(h) = self.cachelito.health(key_bytes) {
                if h.is_circuit_open() {
                    continue;
                }
            }
            let tier = self.tier_for(tier_id);
            match tier.get(key_bytes) {
                Ok(Some(v)) => return Ok(v),
                Ok(None) => continue,
                Err(_) => continue,
            }
        }
        Err(CacheError::PopulationFailed)
    }
}
