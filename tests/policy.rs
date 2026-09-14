#![allow(unused_imports)]
mod common;

use std::sync::Arc;

use thesix::{
    CacheError, CacheManager, CacheOperation, CachePolicy, CacheRequest, CacheState, Cachelito,
    DefaultPolicy, IdentityContext, KeyRef, L0Stub, L1Stub, L2Stub, L3Stub, L4Stub, L5Stub,
    MemoryPool, PolicyDecision, TierId, TierRegistry,
};

use common::{make_manager, test_ctx};

#[derive(Debug, Clone)]
struct DenyPolicy;

impl<K, V> CachePolicy<K, V> for DenyPolicy {
    fn select(
        &self,
        _request: &thesix::CacheRequest<K, V>,
        _state: &thesix::CacheState,
        _identity: &IdentityContext,
    ) -> PolicyDecision {
        PolicyDecision::deny()
    }
}

#[tokio::test]
async fn test_policy_selection() {
    let policy = DefaultPolicy;

    let request_set: CacheRequest<&str, &str> = CacheRequest::new(CacheOperation::Set, "key");
    let request_get: CacheRequest<&str, &str> = CacheRequest::new(CacheOperation::Get, "key");
    let state = CacheState::new();
    let identity = IdentityContext::anonymous();

    let set_decision = policy.select(&request_set, &state, &identity);
    assert_eq!(set_decision.tier, TierId::L1);
    assert!(set_decision.authorized);

    let get_decision = policy.select(&request_get, &state, &identity);
    assert!(get_decision.authorized);

    let authenticated =
        IdentityContext::new("principal".into(), vec!["role".into()], "tenant".into());
    let get_decision_auth = policy.select(&request_get, &state, &authenticated);
    assert!(get_decision_auth.authorized);
    assert_eq!(get_decision_auth.tier, TierId::L0);
}

#[tokio::test]
async fn test_policy_replacement() {
    let manager = make_manager(DefaultPolicy);
    let key = "policy-key".to_string();

    manager
        .set(&key, "value".to_string(), &test_ctx())
        .await
        .unwrap();

    let result = manager.get(&key, &test_ctx()).await.unwrap();
    assert_eq!(result, Some("value".to_string()));

    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let tiers: Vec<Arc<dyn thesix::CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    let manager_deny = CacheManager::new(DenyPolicy, cachelito, tier_registry, tiers, pool);

    let result = manager_deny.get(&key, &test_ctx()).await;
    assert!(matches!(result, Err(CacheError::Unauthorized)));

    let result = manager_deny
        .set(&key, "value".to_string(), &test_ctx())
        .await;
    assert!(matches!(result, Err(CacheError::Unauthorized)));
}
