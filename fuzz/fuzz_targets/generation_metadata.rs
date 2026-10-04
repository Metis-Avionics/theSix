//! Trust boundary: generation and version metadata.
//!
//! Property: a publish must never be accepted for a generation that is not the
//! current one, in either direction — neither stale nor impossibly far ahead.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thesix::{Generation, IntentKind, TierId};

fuzz_target!(|data: &[u8]| {
    let cachelito = thesix::Cachelito::new();
    let Some(&raw) = data.first() else { return };
    let token = match cachelito.prepare(b"k", None, TierId::L1, IntentKind::Write) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Commit at the exact generation.
    assert!(cachelito.commit(&token, None).is_ok());

    // Commit at an arbitrary offset must fail: `publish` compares for exact
    // equality, so both a stale and an ahead token are rejected.
    let offset = Generation::new(u64::from(raw).saturating_add(1));
    let stale = thesix::CommitToken {
        key_hash: token.key_hash,
        generation: offset,
        kind: token.kind,
        target_tier: token.target_tier,
    };
    assert_eq!(
        cachelito.commit(&stale, None),
        Err(thesix::CacheError::StaleGeneration),
        "a token at the wrong generation was committed"
    );

    // And `is_stale` is a strict ordering over a range that cannot wrap.
    //
    // The base is offset by one so `base - 1` is always a *smaller* generation. An
    // earlier version compared `raw` against `raw.wrapping_sub(1)`, which at
    // `raw == 0` yields `u64::MAX` -- newer, not older -- and asserted that it was
    // stale. The fuzzer found it on the first case. `u64` is not circular, and
    // neither is the ordering under test.
    let base = Generation::new(u64::from(raw).saturating_add(1));
    let older = Generation::new(base.0 - 1);
    assert!(older.is_stale(base), "{} is not older than {}", older.0, base.0);
    assert!(!base.is_stale(older), "the relation is not antisymmetric");
    assert!(!base.is_stale(base), "a generation is stale against itself");
    assert!(
        !Generation::new(base.0.saturating_add(1)).is_stale(base),
        "a newer generation reported itself stale"
    );
});
