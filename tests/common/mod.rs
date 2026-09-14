//! Shared test helpers: an authenticated request context and manager builders.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use thesix::{
    CacheContext, CacheManager, Cachelito, DefaultPolicy, IdentityContext, L0Stub, L1Stub, L2Stub,
    L3Stub, L4Stub, L5Stub, MemoryPool, TierRegistry,
};

/// An authenticated context for tests that exercise the happy path.
pub fn test_ctx() -> CacheContext {
    CacheContext::new(IdentityContext::new(
        "test-principal".to_string(),
        vec!["tester".to_string()],
        "test-tenant".to_string(),
    ))
}

/// An anonymous context, for authn/authz negative tests.
pub fn anon_ctx() -> CacheContext {
    CacheContext::anonymous()
}

pub fn make_manager<V: Clone + Send + Sync + 'static>(
    policy: DefaultPolicy,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let tiers: Vec<Arc<dyn thesix::CacheTier<V>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    Arc::new(CacheManager::new(
        policy,
        cachelito,
        tier_registry,
        tiers,
        pool,
    ))
}

pub fn make_manager_with_timeout<V: Clone + Send + Sync + 'static>(
    policy: DefaultPolicy,
    timeout: Duration,
) -> Arc<CacheManager<String, V, DefaultPolicy>> {
    let cachelito = Cachelito::new();
    let tier_registry = TierRegistry::new();
    let tiers: Vec<Arc<dyn thesix::CacheTier<V>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    let pool = MemoryPool::new(1024).expect("MemoryPool allocation failed");
    Arc::new(CacheManager::with_timeout(
        policy,
        cachelito,
        tier_registry,
        tiers,
        pool,
        timeout,
    ))
}
