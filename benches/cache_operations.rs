#![allow(unused_imports)]
use criterion::{Criterion, criterion_group, criterion_main};
use std::sync::Arc;

use thesix::{
    CacheManager, CachePolicy, Cachelito, DefaultPolicy, KeyRef, L0Stub, L1Stub, L2Stub, L3Stub,
    L4Stub, L5Stub, MemoryPool, TierRegistry,
};

fn make_manager() -> Arc<CacheManager<String, String, DefaultPolicy>> {
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
    Arc::new(CacheManager::new(
        DefaultPolicy,
        cachelito,
        tier_registry,
        tiers,
        pool,
    ))
}

fn bench_uncontended_get(c: &mut Criterion) {
    let manager = make_manager();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ctx = thesix::CacheContext::new(thesix::IdentityContext::new(
        "bench".to_string(),
        vec![],
        "bench".to_string(),
    ));
    c.bench_function("uncontended_get", |b| {
        b.iter(|| {
            let m = Arc::clone(&manager);
            let ctx = ctx.clone();
            rt.block_on(async move {
                m.set(&"key".to_string(), "value".to_string(), &ctx)
                    .await
                    .unwrap();
                m.get(&"key".to_string(), &ctx).await.unwrap();
            });
        });
    });
}

fn bench_cachelito_lookup(c: &mut Criterion) {
    let cachelito = Arc::new(Cachelito::new());
    let key = Arc::new(b"bench-key".to_vec());
    let rt = tokio::runtime::Runtime::new().unwrap();
    c.bench_function("cachelito_lookup", |b| {
        b.iter(|| {
            let c = Arc::clone(&cachelito);
            let k = Arc::clone(&key);
            rt.block_on(async move {
                let _ = c.acquire(&k, thesix::TierId::L0);
            });
        });
    });
}

fn bench_policy_evaluation(c: &mut Criterion) {
    let policy = DefaultPolicy;
    let rt = tokio::runtime::Runtime::new().unwrap();
    c.bench_function("policy_evaluation", |b| {
        b.iter(|| {
            let policy = policy.clone();
            let request: thesix::CacheRequest<&str, &str> =
                thesix::CacheRequest::new(thesix::CacheOperation::Get, "key");
            let state = thesix::CacheState::new();
            let identity = thesix::IdentityContext::anonymous();
            rt.block_on(async move {
                let _ = policy.select(&request, &state, &identity);
            });
        });
    });
}

criterion_group!(
    benches,
    bench_uncontended_get,
    bench_cachelito_lookup,
    bench_policy_evaluation
);
criterion_main!(benches);
