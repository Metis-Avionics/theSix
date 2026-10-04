//! Trust boundary: payloads.
//!
//! Property: a payload of arbitrary bytes must round-trip through a tier without
//! panic and without silent alteration, and a *damaged* payload must be refused
//! rather than served.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thesix::{CacheError, FixedTierStub, KeyRef};

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let mut stub = FixedTierStub::<Vec<u8>>::with_capacity(4).expect("stub");
        let key = KeyRef(b"payload");

        stub.set(&key, data.to_vec(), None).expect("set");
        let got = stub.get(&key).expect("get");
        assert_eq!(got.as_deref(), Some(data), "payload did not round-trip");

        // Damage it and require refusal. If a mutation happens to leave the value
        // identical, skip rather than assert — the point is that a *changed*
        // value is never served.
        if !data.is_empty() {
            let mut damaged = data.to_vec();
            let last = damaged.len() - 1;
            damaged[last] ^= 0xFF;
            if damaged != data {
                stub.corrupt_stored_value_for_test(&key, damaged)
                    .expect("corrupt");
                match stub.get(&key) {
                    Err(CacheError::Corrupted) => {}
                    other => panic!("a damaged payload was served: {other:?}"),
                }
            }
        }
    });
});
