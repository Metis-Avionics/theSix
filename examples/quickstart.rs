//! Quick start for `thesix` — mirrors the README example.
//!
//! Run with: `cargo run --example quickstart`

use std::sync::Arc;

use thesix::{
    CacheContext, CacheManager, CacheTier, Cachelito, DefaultPolicy, IdentityContext, L0Stub,
    L1Stub, L2Stub, L3Stub, L4Stub, L5Stub, MemoryPool, TierRegistry,
};

#[tokio::main]
async fn main() -> Result<(), thesix::CacheError> {
    // Six tiers, dumb by design: policy + Cachelito decide everything.
    let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    let manager: CacheManager<String, String, DefaultPolicy> = CacheManager::new(
        DefaultPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        tiers,
        MemoryPool::new(1024).expect("pool allocation failed"),
    );

    // One request context per call site: identity + optional TTL.
    let ctx = CacheContext::new(IdentityContext::new(
        "alice".to_string(),
        vec!["reader".to_string()],
        "tenant-1".to_string(),
    ))
    .with_ttl(std::time::Duration::from_secs(60));

    let key = "my-key".to_string();

    // Single-flight: 100 concurrent callers → exactly one fetch.
    let value = manager
        .get_or_fetch(&key, &ctx, || async { Ok("value".to_string()) })
        .await?;
    assert_eq!(value, "value");

    manager.set(&key, "new-value".to_string(), &ctx).await?;
    assert!(manager.exists(&key, &ctx).await?);
    manager.invalidate(&key, &ctx).await?;
    Ok(())
}
