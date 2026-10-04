//! Trust boundary: routing decisions.
//!
//! Property: no routing decision may ever place data on a rung outside the cache
//! ladder, and the fail-open fallback scan must never surface the authority rung.
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::sync::Arc;
use std::time::Duration;

use thesix::{CacheTier, L0Stub, L1Stub, TierId};

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        // A random prefix of the ladder, so every topology gets exercised.
        let n = data.first().copied().unwrap_or(0) as usize % 7;
        let mut tiers: Vec<Arc<dyn CacheTier<String>>> = Vec::new();
        for _ in 0..n {
            tiers.push(Arc::new(L0Stub::<String>::new()));
        }
        if n > 1 {
            tiers[1] = Arc::new(L1Stub::<String>::new());
        }
        let m = testkit::manager_from_tiers(
            thesix::DefaultPolicy,
            tiers,
            Duration::from_millis(60),
        );
        let ctx = testkit::test_ctx();

        for i in 0..4 {
            let key = format!("k{}", data.get(i).copied().unwrap_or(0) as usize % 8);
            let _ = m.set(&key, "v".to_string(), &ctx).await;
            let _ = m.get(&key, &ctx).await;
            let _ = m.promote(&key, &ctx).await;
            let _ = m.demote(&key, &ctx).await;

            let snap = m.cachelito().peek(&testkit::framed_key(&ctx, &key));
            if let Ok(snap) = snap {
                assert!(
                    snap.tier.is_cache_rung(),
                    "routing placed an entry on {}, outside the ladder",
                    snap.tier
                );
                assert_ne!(snap.tier, TierId::L6, "routing selected the authority rung");
            }
        }

        // The ladder constant must agree with the tier enum.
        assert_eq!(
            thesix::policy::LAST_CACHE_TIER.as_usize(),
            thesix::tier::LAST_CACHE_RUNG_INDEX
        );
        for tier in thesix::policy::cache_ladder() {
            assert!(tier.is_cache_rung());
            assert_ne!(tier, TierId::L6);
        }
    });
});
