//! ETL with `thesix` + Polars: cache-aside marts over batch extracts.
//!
//! Pipeline: **Extract** a synthetic orders batch into a Polars `DataFrame`,
//! **Transform** it (revenue by region), **Load** the result into the cache
//! with [`CacheManager::get_or_fetch`]. Demonstrates:
//!
//! - single-flight populate: 20 concurrent readers → exactly one transform
//! - generation invalidation: a new batch invalidates the old mart key
//!
//! Run with:
//! `cargo run --example polars_etl` (needs `--all-features` for dev-deps)

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use polars::prelude::*;
use thesix::{
    CacheContext, CacheError, CacheManager, CacheTier, Cachelito, DefaultPolicy, IdentityContext,
    L0Stub, L1Stub, L2Stub, L3Stub, L4Stub, L5Stub, MemoryPool, TierRegistry,
};

type OrdersCache = CacheManager<String, String, DefaultPolicy>;

fn make_manager() -> OrdersCache {
    let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    CacheManager::new(
        DefaultPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        tiers,
        MemoryPool::new(1024).expect("pool allocation failed"),
    )
}

fn ctx() -> CacheContext {
    CacheContext::new(IdentityContext::new(
        "etl-worker".to_string(),
        vec!["reader".to_string(), "writer".to_string()],
        "analytics".to_string(),
    ))
    .with_ttl(std::time::Duration::from_secs(300))
}

/// Extract: synthetic orders batch. `batch_no` selects the data vintage.
fn extract(batch_no: u32) -> Result<DataFrame, PolarsError> {
    match batch_no {
        1 => df!(
            "region" => ["north", "south", "north", "east"],
            "amount" => [10_i64, 20, 30, 40],
        ),
        _ => df!(
            "region" => ["north", "south", "north", "east", "north", "west"],
            "amount" => [10_i64, 20, 30, 40, 50, 60],
        ),
    }
}

/// Transform: revenue per region. The counter proves how often this ran.
fn transform(frame: &DataFrame, runs: &AtomicUsize) -> Result<String, CacheError> {
    runs.fetch_add(1, Ordering::SeqCst);
    let agg = frame
        .clone()
        .lazy()
        .group_by([col("region")])
        .agg([col("amount").sum().alias("revenue")])
        .sort(["region"], SortMultipleOptions::default())
        .collect()
        .map_err(|_| CacheError::PopulationFailed)?;
    Ok(format!("{agg}"))
}

#[tokio::main]
async fn main() -> Result<(), CacheError> {
    let manager = Arc::new(make_manager());
    let context = ctx();
    let transform_runs = Arc::new(AtomicUsize::new(0));

    // Load batch 1 under a versioned mart key. 20 concurrent readers
    // stampede the same missing key; single-flight runs one transform.
    let mart_key = "mart:revenue_by_region:v1".to_string();
    let mut handles = Vec::new();
    for _ in 0..20 {
        let m = Arc::clone(&manager);
        let c = context.clone();
        let k = mart_key.clone();
        let runs = Arc::clone(&transform_runs);
        handles.push(tokio::spawn(async move {
            m.get_or_fetch(&k, &c, || async {
                let frame = extract(1).map_err(|_| CacheError::PopulationFailed)?;
                transform(&frame, &runs)
            })
            .await
        }));
    }
    let mut first: Option<String> = None;
    for h in handles {
        let value = h.await.expect("reader task panicked")?;
        if let Some(prev) = first.replace(value.clone()) {
            assert_eq!(prev, value, "all readers must see the same mart");
        }
    }
    let mart = first.expect("at least one reader");
    assert_eq!(
        transform_runs.load(Ordering::SeqCst),
        1,
        "single-flight: one transform for 20 readers"
    );
    println!("batch-1 mart (computed once):\n{mart}");
    assert!(mart.contains("40") && mart.contains("north"));

    // New batch lands: invalidate the old generation so the next read
    // recomputes instead of serving the stale mart.
    manager.invalidate(&mart_key, &context).await?;
    let mart2 = manager
        .get_or_fetch(&mart_key, &context, || async {
            let frame = extract(2).map_err(|_| CacheError::PopulationFailed)?;
            transform(&frame, &transform_runs)
        })
        .await?;
    assert_eq!(
        transform_runs.load(Ordering::SeqCst),
        2,
        "invalidation forced exactly one recompute"
    );
    println!("batch-2 mart (recomputed once):\n{mart2}");
    assert!(mart2.contains("90") && mart2.contains("west"));
    Ok(())
}
