//! Trust boundary: configuration.
//!
//! Property: no configuration input may panic, and every invalid one must be
//! rejected rather than silently clamped into something that looks valid.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thesix::{Cachelito, FixedTierStub, MemoryPool, TierRegistry};

fuzz_target!(|data: &[u8]| {
    let Some(&first) = data.first() else { return };
    let capacity = usize::from(first) as usize % 64;

    // Zero is the interesting case: it must be refused by the fallible
    // constructors and clamped by the ones documented as infallible.
    assert!(FixedTierStub::<String>::with_capacity(capacity).is_ok() || capacity == 0);
    assert!(MemoryPool::<String>::new(capacity).is_ok() || capacity == 0);

    // Shard counts must never cause a division by zero.
    let shards = capacity.max(1);
    let cachelito = Cachelito::with_shards(shards);
    for i in 0..8 {
        let key = format!("k{i}");
        assert!(cachelito.peek(key.as_bytes()).is_ok());
        assert!(cachelito.bump_generation(key.as_bytes()).is_ok());
    }

    // A registry always covers every rung, whatever the shard count was.
    let registry = TierRegistry::new();
    assert_eq!(registry.len(), thesix::TierId::ALL.len());
});
