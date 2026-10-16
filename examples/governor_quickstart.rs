//! The governor, end to end.
//!
//! Assemble the six-rung ladder from the environment (`THESIX_REDIS_URL`,
//! `THESIX_VALKEY_URL`, `REDIS_URL` or `VALKEY_URL` — a `.env` file is loaded
//! when the `dotenv` feature is on), register the keys autonomy may move, and
//! let a fully-autonomous governor drive sweeps, rebinding and rebalancing.
//!
//! Run with a valkey or redis server reachable from the configured URL; without
//! one, L3 degrades to its stub and the governor rebinds it when it appears.

use std::sync::Arc;
use std::time::Duration;

use thesix::{
    CacheContext, CacheManager, DefaultPolicy, Governor, GovernorMode, GovernorSettings,
    IdentityContext, KeyCensus, MemoryPool, RingTelemetry, TierRegistry,
};

#[tokio::main]
async fn main() -> Result<(), thesix::CacheError> {
    let env = thesix::governor::ProcessEnv::load_dotenv();
    let ladder = thesix::governor::assemble_ladder::<Vec<u8>>(&env)?;
    println!("l3 binding: {:?}", ladder.report.l3);

    let ring = Arc::new(RingTelemetry::new(1024));
    let manager: Arc<CacheManager<Vec<u8>, Vec<u8>, DefaultPolicy>> = Arc::new(
        CacheManager::new(
            DefaultPolicy,
            thesix::Cachelito::new(),
            TierRegistry::new(),
            ladder.tiers,
            MemoryPool::new(1024)?,
        )
        .with_shared_telemetry(ring.clone()),
    );

    // The census is the boundary crossing the other way: these are the keys
    // autonomy is allowed to move, declared by the application that owns them.
    let census = Arc::new(KeyCensus::new(CacheContext::new(IdentityContext::new(
        "governor-ops".to_string(),
        vec!["maintenance".to_string()],
        "tenant-quickstart".to_string(),
    ))));
    for key in ["order-1", "order-2", "order-3"] {
        census
            .register(key.to_string().into_bytes())
            .expect("quickstart keys fit");
    }

    let mut governor = Governor::new(
        Arc::clone(&manager),
        GovernorSettings {
            mode: GovernorMode::FullyAutonomous,
            interval: Duration::from_secs(1),
            hot_heat: 2,
            ..GovernorSettings::default()
        },
    )
    .with_census(census)
    .with_heat(ring.clone());
    if let Some(rebinder) = ladder.rebinder {
        governor = governor.with_rebinder(rebinder);
    }
    for hot in ladder.hot_rungs {
        governor = governor.with_hot_rung(hot);
    }

    let handle = Arc::new(governor).spawn()?;

    // Some traffic on the same tenant the census frames: heat builds up in the
    // ring, and the governor's ticks move hot keys up and cold keys down.
    let ctx = CacheContext::new(IdentityContext::new(
        "app".to_string(),
        vec!["reader".to_string()],
        "tenant-quickstart".to_string(),
    ));
    for i in 0..6_u64 {
        let key: Vec<u8> = format!("order-{}", (i % 3) + 1).into_bytes();
        let _value = manager
            .get_or_fetch(&key, &ctx, || async {
                Ok::<_, thesix::CacheError>(format!("payload-{i}").into_bytes())
            })
            .await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // The loop stops; any tick in flight unwinds through the manager's guards.
    handle.abort();
    handle.join().await;

    Ok(())
}
