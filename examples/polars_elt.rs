//! ELT with `thesix` + Polars: load raw, transform on read.
//!
//! The raw orders table lives in the "lake" (here an in-memory frame behind
//! a lock; think object store in production). Marts are computed at query
//! time via [`CacheManager::get_or_fetch`] and revalidated with
//! [`CacheManager::refresh`], which serves the stale mart when the refresh
//! itself fails — e.g. a broken transform deploy.
//!
//! Run with:
//! `cargo run --example polars_elt` (needs `--all-features` for dev-deps)

use std::sync::Arc;

use polars::prelude::*;
use thesix::{
    CacheContext, CacheError, CacheManager, CacheTier, Cachelito, DefaultPolicy, IdentityContext,
    L0Stub, L1Stub, L2Stub, L3Stub, L4Stub, L5Stub, MemoryPool, TierRegistry,
};
use tokio::sync::Mutex;

type Marts = CacheManager<String, String, DefaultPolicy>;

fn make_manager() -> Marts {
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
        "elt-service".to_string(),
        vec!["reader".to_string(), "writer".to_string()],
        "analytics".to_string(),
    ))
}

/// Transform: revenue by region over whatever is currently in the lake.
async fn revenue_by_region(lake: &Mutex<DataFrame>) -> Result<String, CacheError> {
    let frame = lake.lock().await.clone();
    let agg = frame
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
    let manager = make_manager();
    let context = ctx();
    let mart_key = "mart:revenue_by_region".to_string();

    // Load: batch 1 lands in the lake.
    let lake = Arc::new(Mutex::new(
        df!(
            "region" => ["north", "south", "north"],
            "amount" => [10_i64, 20, 30],
        )
        .expect("batch-1 frame"),
    ));

    // Transform on read: first query computes and caches the mart.
    let lake_r = Arc::clone(&lake);
    let mart = manager
        .get_or_fetch(&mart_key, &context, || async {
            revenue_by_region(&lake_r).await
        })
        .await?;
    println!("mart from batch 1:\n{mart}");
    assert!(mart.contains("40")); // north = 10 + 30

    // Batch 2 lands in the lake; the cached mart is now stale.
    let batch2 = df!(
        "region" => ["west"],
        "amount" => [50_i64],
    )
    .expect("batch-2 frame");
    let stacked = lake
        .lock()
        .await
        .clone()
        .vstack(&batch2)
        .expect("append batch 2");
    *lake.lock().await = stacked;

    // A broken transform deploy fails the refresh — the stale mart is
    // served instead of an error, keeping dashboards alive.
    let stale = manager
        .refresh(&mart_key, &context, || async {
            Err::<String, CacheError>(CacheError::PopulationFailed)
        })
        .await?
        .expect("stale mart served on refresh failure");
    assert_eq!(stale, mart, "refresh failure serves the previous mart");
    println!("refresh with broken transform still serves:\n{stale}");

    // Fixed deploy: refresh revalidates and returns the fresh mart.
    let lake_r = Arc::clone(&lake);
    let fresh = manager
        .refresh(&mart_key, &context, || async {
            revenue_by_region(&lake_r).await
        })
        .await?
        .expect("fresh mart after successful refresh");
    println!("mart after successful refresh:\n{fresh}");
    assert!(fresh.contains("50") && fresh.contains("west"));
    assert_ne!(fresh, mart);
    Ok(())
}
