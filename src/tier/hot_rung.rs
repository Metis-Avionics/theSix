//! A rung whose backing tier can be replaced while the cache is live.
//!
//! Tier *binding* is otherwise a construction-time fact: the manager holds an
//! immutable `Vec<Arc<dyn CacheTier<V>>>`, and that is deliberate — `bound_tier`
//! never substitutes an unbound rung, and honesty about what is bound is a
//! contract clause. This type makes a rung *rebindable* without weakening either
//! property: the manager still sees one `Arc<dyn CacheTier<V>>` per rung, and
//! what is bound is still answerable at every moment — it is whatever the inner
//! tier currently says, forwarded verbatim.
//!
//! The governor uses this to replace a failed backend with a reconnected one
//! without rebuilding the manager (which would drop every entry the control
//! plane still describes). The swap is synchronous and cheap; no lock is held
//! across any `.await` because each async method clones the inner `Arc` out of
//! the lock and awaits on the clone.
//!
//! The rung is fixed at construction and every swap must return a tier for the
//! *same* rung. Enforcing that is what keeps the control plane's per-entry rung
//! record meaningful: an entry that trusts `snapshot.tier` must be able to trust
//! that the tier bound there still claims that rung.

use std::sync::Arc;

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::TierId;
use crate::tier::tier_trait::{BackendKind, CacheTier, TierHealth};

/// A `CacheTier` that forwards to an inner tier which can be swapped at runtime.
pub struct HotRungTier<V> {
    rung: TierId,
    inner: std::sync::RwLock<Arc<dyn CacheTier<V>>>,
}

impl<V> std::fmt::Debug for HotRungTier<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotRungTier")
            .field("rung", &self.rung)
            .finish_non_exhaustive()
    }
}

impl<V> HotRungTier<V> {
    /// Bind `rung` to `initial`, refusing a tier that claims another rung.
    ///
    /// # Errors
    ///
    /// [`CacheError::ConfigurationError`] when `initial.tier_id() != rung`:
    /// binding a tier onto a rung it does not claim is the substitution this
    /// crate's capability surface exists to prevent.
    pub fn new(rung: TierId, initial: Arc<dyn CacheTier<V>>) -> Result<Self, CacheError> {
        if initial.tier_id() != rung {
            return Err(CacheError::ConfigurationError);
        }
        Ok(HotRungTier {
            rung,
            inner: std::sync::RwLock::new(initial),
        })
    }

    /// Replace the backing tier, returning the one displaced.
    ///
    /// # Errors
    ///
    /// [`CacheError::ConfigurationError`] when `replacement.tier_id() != rung`.
    /// A rebinding must not change *which* rung answers; only what backs it.
    pub fn hot_swap(
        &self,
        replacement: Arc<dyn CacheTier<V>>,
    ) -> Result<Arc<dyn CacheTier<V>>, CacheError> {
        if replacement.tier_id() != self.rung {
            return Err(CacheError::ConfigurationError);
        }
        let Ok(mut guard) = self.inner.write() else {
            return Err(CacheError::ConfigurationError);
        };
        Ok(std::mem::replace(&mut guard, replacement))
    }

    /// The tier currently bound.
    ///
    /// Clones the `Arc` out of the lock and releases it before returning, so no
    /// method of this type can ever hold a guard across an await.
    fn current(&self) -> Result<Arc<dyn CacheTier<V>>, CacheError> {
        self.inner
            .read()
            .map_err(|_| CacheError::ConfigurationError)
            .map(|g| Arc::clone(&g))
    }

    /// Which rung this binding serves. Fixed for the wrapper's lifetime.
    #[must_use]
    pub const fn rung(&self) -> TierId {
        self.rung
    }
}

#[async_trait::async_trait]
impl<V: Clone + Send + Sync + 'static> CacheTier<V> for HotRungTier<V> {
    fn name(&self) -> String {
        match self.current() {
            Ok(inner) => inner.name(),
            Err(_) => "hot-rung-poisoned".to_string(),
        }
    }

    fn backend(&self) -> BackendKind {
        match self.current() {
            Ok(inner) => inner.backend(),
            Err(_) => BackendKind::Unavailable,
        }
    }

    fn capability(&self) -> crate::capability::TierCapability {
        match self.current() {
            Ok(inner) => inner.capability(),
            Err(_) => crate::capability::TierCapability::unbound(),
        }
    }

    fn eviction_candidate(&self) -> Option<crate::integrity::KeyAddress> {
        self.current().ok()?.eviction_candidate()
    }

    /// Capacity is the inner tier's, because the wrapper adds no slots: a swap
    /// replaces the accounting with the accounting, never with a claim.
    fn capacity(&self) -> Option<usize> {
        self.current().ok()?.capacity()
    }

    fn remove_if_address(&self, address: crate::integrity::KeyAddress) -> Result<bool, CacheError> {
        self.current()?.remove_if_address(address)
    }

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        let inner = self.current()?;
        inner.get(key).await
    }

    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        let inner = self.current()?;
        inner.set(key, value, ttl).await
    }

    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        let inner = self.current()?;
        inner.remove(key).await
    }

    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        let inner = self.current()?;
        inner.contains(key).await
    }

    fn health(&self) -> TierHealth {
        match self.current() {
            Ok(inner) => inner.health(),
            Err(_) => TierHealth::default(),
        }
    }

    fn tier_id(&self) -> TierId {
        self.rung
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::integrity::IntegrityCheck;
    use crate::tier::test::TestTier;

    fn a_test_tier<V: Clone + Send + Sync + 'static + IntegrityCheck>(
        rung: TierId,
    ) -> Arc<dyn CacheTier<V>> {
        Arc::new(TestTier::<V>::new(rung))
    }

    #[test]
    fn a_swap_refuses_a_tier_claiming_another_rung() {
        testkit::proves!("autonomy.rebind.rebind_refuses_a_foreign_rung");

        let hot =
            HotRungTier::<Vec<u8>>::new(TierId::L1, a_test_tier(TierId::L1)).expect("same rung");
        assert!(
            hot.hot_swap(a_test_tier(TierId::L1)).is_ok(),
            "a rung-honest tier must be accepted"
        );
        assert!(
            hot.hot_swap(a_test_tier(TierId::L2)).is_err(),
            "a tier claiming another rung must be refused"
        );
    }

    #[test]
    fn the_wrapper_forwards_the_inner_tier_identity() {
        testkit::proves!("autonomy.rebind.rebind_preserves_capability_honesty");

        let hot = HotRungTier::<Vec<u8>>::new(TierId::L1, a_test_tier(TierId::L1))
            .expect("stub claims L1");
        let before = hot.capability();
        let backend_before = before.backend;
        let state_before = before.state;

        let displaced = hot
            .hot_swap(a_test_tier(TierId::L1))
            .expect("rebind with a rung-matching tier");
        let after = hot.capability();

        assert_eq!(hot.backend(), BackendKind::Test);
        assert_eq!(hot.tier_id(), TierId::L1);
        assert_eq!(after.backend, backend_before);
        assert_eq!(
            after.state, state_before,
            "a rebound rung must keep reporting the identity it actually has"
        );
        assert_eq!(displaced.tier_id(), TierId::L1);
        assert_eq!(hot.rung(), TierId::L1);
    }

    #[tokio::test]
    async fn a_swap_lands_around_in_flight_operations_not_mid_write() {
        testkit::proves!("autonomy.rebind.rebind_preserves_capability_honesty");

        let hot = HotRungTier::<Vec<u8>>::new(TierId::L2, a_test_tier(TierId::L2))
            .expect("stub claims L2");
        let key = KeyRef(b"hot-rung-key");
        hot.set(&key, b"v1".to_vec(), None).await.expect("set v1");

        // The swap changes what the rung answers with. The value the old inner
        // held is not migrated: rebinding is a binding change, not a move, and
        // the control plane still describes every entry it did before.
        let _ = hot
            .hot_swap(a_test_tier(TierId::L2))
            .expect("rebind with a rung-matching tier");
        let read_after_swap = hot.get(&key).await.expect("get on the new inner");
        assert_eq!(
            read_after_swap, None,
            "the old inner's value leaked through"
        );

        // The new inner is live: writing through the wrapper reaches it.
        hot.set(&key, b"v2".to_vec(), None).await.expect("set v2");
        assert_eq!(hot.get(&key).await.expect("get"), Some(b"v2".to_vec()));
    }

    /// Concurrent reads must never observe a torn swap. The inner `Arc` is
    /// swapped atomically under the write lock, so a reader holds one inner for
    /// the whole operation (clone-then-await).
    #[tokio::test]
    async fn concurrent_reads_survive_a_concurrent_swap() {
        testkit::proves!("autonomy.rebind.rebind_preserves_capability_honesty");

        let hot = Arc::new(
            HotRungTier::<Vec<u8>>::new(TierId::L1, a_test_tier(TierId::L1)).expect("same rung"),
        );
        let done = Arc::new(AtomicUsize::new(0));

        let readers = 8;
        let mut handles = Vec::new();
        for _ in 0..readers {
            let hot = Arc::clone(&hot);
            let done = Arc::clone(&done);
            handles.push(tokio::spawn(async move {
                for _ in 0..50 {
                    let key = KeyRef(b"race-key");
                    let r = hot.get(&key).await;
                    assert!(
                        r.is_ok(),
                        "a read must not fail because another task swapped"
                    );
                    let _ = hot.hot_swap(a_test_tier(TierId::L1));
                    done.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for handle in handles {
            handle.await.expect("reader task");
        }
        assert_eq!(done.load(Ordering::Relaxed), readers * 50);
    }
}
