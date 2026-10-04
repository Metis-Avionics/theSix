//! Trust boundary: backend responses.
//!
//! Property: a tier that returns arbitrary shaped results must not let the
//! manager misreport them. In particular a `Corrupted` from the data plane must
//! not be presented to the caller as a value or as an ordinary miss.
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::sync::Arc;
use std::time::Duration;

use testkit::FaultyTier;
use thesix::FaultPlan;
use thesix::{CacheError, CacheTier, FaultClass, L0Stub, OpKind};

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        // Pick a fault class from the input, arm it, and require that the error
        // the caller sees is the one the fault produces.
        let classes = [
            FaultClass::Timeout,
            FaultClass::ReadFailure,
            FaultClass::WriteFailure,
            FaultClass::Disconnect,
            FaultClass::CapacityExhaustion,
            FaultClass::Corruption,
            FaultClass::MetadataFailure,
        ];
        let class = classes[data.first().copied().unwrap_or(0) as usize % classes.len()];

        let tier = FaultyTier::<String>::with_plan(
            Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
            FaultPlan::new().push(OpKind::Get, class),
        );
        let ledger = tier.ledger();
        let m = testkit::manager_from_parts(
            thesix::DefaultPolicy,
            thesix::Cachelito::new(),
            vec![Arc::clone(&tier) as Arc<dyn CacheTier<String>>],
            Duration::from_millis(80),
        );
        let ctx = testkit::test_ctx();

        let result = m.get(&format!("k{}", data.len()), &ctx).await;
        if ledger.fired(class) > 0 {
            // The fault fired, so the caller must not have seen a value.
            assert!(
                !matches!(result, Ok(Some(_))),
                "a fired {class} still produced a value: {result:?}"
            );
            // And corruption must surface as corruption, never as a plain miss.
            if class == FaultClass::Corruption {
                assert!(
                    matches!(result, Err(CacheError::Corrupted)),
                    "corruption was reported as {result:?}"
                );
            }
        }
    });
});
