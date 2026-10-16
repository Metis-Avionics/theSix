//! The movable key set.
//!
//! The control plane stores only key hashes and the tiers store no key material
//! at all, so an autonomous loop cannot discover *which* key lives where — by
//! design. The census is the boundary crossing the other way: the application
//! declares which keys autonomy may act on, and the governor does the rest.
//!
//! Keys registered here are retained *by the application's choice*, in the same
//! sense that the application retains them anywhere else; nothing new is
//! retained by the control plane or by the tiers. The census derives, once, the
//! tenant-framed bytes and the [`KeyIdentity`] each operation will carry, so a
//! tick does not re-frame keys and telemetry heat can be matched to census
//! entries without ever holding a mapping outside this type.
//!
//! The census carries the maintenance [`CacheContext`]: autonomy acts on entries
//! under this identity, so it is subject to the same authentication gate as any
//! consumer call. An unauthenticated census makes every planned move fail with
//! `Unauthenticated` — counted, not swallowed.

use crate::error::CacheError;
use crate::identity::CacheContext;
use crate::key::Key;
use crate::telemetry::KeyIdentity;

/// The framing budget the manager itself encodes keys into.
///
/// Mirrored from `CacheManager`'s private constant: a census entry whose frame
/// exceeds it could never be encoded by a manager operation, so registering one
/// must fail here rather than fail mid-tick on every move.
const MANAGER_KEY_BUDGET: usize = 256;

/// One movable key, with its derived identity.
#[derive(Debug, Clone)]
pub struct CensusKey<K> {
    pub key: K,
    /// The tenant-framed bytes every manager operation encodes this key into.
    pub framed: Vec<u8>,
    /// The identity the telemetry records for this key, derived from `framed`.
    pub identity: KeyIdentity,
}

/// The set of keys autonomy may move, under one maintenance identity.
pub struct KeyCensus<K> {
    context: CacheContext,
    entries: std::sync::RwLock<Vec<CensusKey<K>>>,
}

impl<K: Key> KeyCensus<K> {
    /// Build a census under `context`. The context must be authenticated for
    /// moves to succeed, and its tenant frames every registered key.
    #[must_use]
    pub fn new(context: CacheContext) -> Self {
        KeyCensus {
            context,
            entries: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// Register `key` as movable.
    ///
    /// # Errors
    ///
    /// [`CacheError::ConfigurationError`] when the key does not encode, the
    /// tenant cannot frame it, or the frame exceeds the manager's encoding
    /// budget: such a key could never be moved by any manager operation, so
    /// admitting it would make the census promise what the manager cannot do.
    pub fn register(&self, key: K) -> Result<CensusKey<K>, CacheError> {
        let mut raw = [0_u8; MANAGER_KEY_BUDGET];
        let encoded = key.encode(&mut raw)?;
        let tenant = self.context.identity().tenant.as_str();
        let mut framed = [0_u8; MANAGER_KEY_BUDGET];
        let len = crate::key::frame_tenant_key(tenant, &raw[..encoded], &mut framed)?;
        let framed = framed[..len].to_vec();
        let identity = KeyIdentity::of(&framed);
        let entry = CensusKey {
            key,
            framed,
            identity,
        };
        // A poisoned registry means some caller panicked while holding it. An
        // `Ok` here would hand back an entry the census then forgets, planning
        // moves for indices that no longer exist, so the registration refuses.
        match self.entries.write() {
            Ok(mut entries) => {
                entries.push(entry.clone());
                Ok(entry)
            }
            Err(_) => Err(CacheError::ConfigurationError),
        }
    }

    /// A snapshot of the registered keys, in registration order.
    #[must_use]
    pub fn entries(&self) -> Vec<CensusKey<K>> {
        self.entries.read().map(|e| e.clone()).unwrap_or_default()
    }

    /// The count of registered keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().map_or(0, |e| e.len())
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The maintenance identity all autonomy movement is performed under.
    #[must_use]
    pub fn context(&self) -> &CacheContext {
        &self.context
    }
}

impl<K: Key> Default for KeyCensus<K> {
    fn default() -> Self {
        KeyCensus::new(CacheContext::anonymous())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CacheContext {
        CacheContext::new(crate::identity::IdentityContext::new(
            "governor-ops".to_string(),
            vec!["maintenance".to_string()],
            "tenant-a".to_string(),
        ))
    }

    #[test]
    fn registration_frames_the_key_and_matches_telemetry_identity() {
        testkit::proves!("autonomy.movement.has_no_key_of_its_own");

        let census = KeyCensus::<String>::new(ctx());
        let entry = census.register("order-42".to_string()).expect("register");
        let mut expected = [0_u8; MANAGER_KEY_BUDGET];
        let len =
            crate::key::frame_tenant_key("tenant-a", b"order-42", &mut expected).expect("frame");
        assert_eq!(entry.framed, expected[..len]);
        assert_eq!(entry.identity, KeyIdentity::of(&entry.framed));
    }

    #[test]
    fn two_tenants_do_not_share_a_census_identity() {
        let a = KeyCensus::<String>::new(ctx());
        let b_ctx = CacheContext::new(crate::identity::IdentityContext::new(
            "governor-ops".to_string(),
            vec!["maintenance".to_string()],
            "tenant-b".to_string(),
        ));
        let b = KeyCensus::<String>::new(b_ctx);
        let ea = a.register("k".to_string()).expect("a");
        let eb = b.register("k".to_string()).expect("b");
        assert_ne!(ea.identity, eb.identity, "the tenant frames the identity");
        assert_ne!(ea.framed, eb.framed);
    }

    #[test]
    fn an_oversized_key_is_refused_not_silently_admitted() {
        let census = KeyCensus::<String>::new(ctx());
        let oversized = "x".repeat(MANAGER_KEY_BUDGET);
        assert_eq!(
            census.register(oversized).map(|_| ()).unwrap_err(),
            CacheError::ConfigurationError,
            "a frame the manager itself could not encode must not enter the census"
        );
        assert_eq!(census.len(), 0);
    }

    #[test]
    fn entries_preserve_registration_order() {
        let census = KeyCensus::<String>::new(ctx());
        for i in 0..5 {
            census.register(format!("k{i}")).expect("register");
        }
        let entries = census.entries();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].key, "k0");
        assert_eq!(entries[4].key, "k4");
    }
}
