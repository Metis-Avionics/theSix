//! Trust boundary: control-plane metadata.
//!
//! Property: an arbitrary operation sequence against `Cachelito` must never panic
//! and must always leave the entry in a state the machine can classify.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thesix::{EntryState, IntentKind, RecoveryDirection, TierId};

fuzz_target!(|data: &[u8]| {
    let cachelito = thesix::Cachelito::new();
    // Drive a deterministic op stream from the bytes.
    for (i, byte) in data.iter().enumerate() {
        let key = format!("k{}", *byte as usize % 8);
        let bytes = key.as_bytes();
        match byte % 6 {
            0 => {
                let _ = cachelito.peek(bytes);
            }
            1 => {
                let _ = cachelito.acquire(bytes, TierId::L0);
            }
            2 => {
                if let Ok(token) = cachelito.prepare(bytes, None, TierId::L1, IntentKind::Write) {
                    let _ = cachelito.commit(&token, None);
                }
            }
            3 => {
                if let Ok(token) = cachelito.prepare(bytes, None, TierId::L2, IntentKind::Move) {
                    let _ = cachelito.abort(&token, thesix::CacheError::Cancelled);
                }
            }
            4 => {
                let _ = cachelito.bump_generation(bytes);
            }
            _ => {
                // Sweep for stale intents. Resolution is exercised below, where
                // the key is available.
                let _ = cachelito.stale_intents(0);
                let _ = i;
            }
        }

        // Whatever happened, the entry must be in a classifiable state and must
        // never be left both prepared and readable.
        if let Ok(snapshot) = cachelito.peek(bytes) {
            assert!(
                !(snapshot.state == EntryState::Prepared && snapshot.state.is_readable()),
                "a prepared entry reported itself readable"
            );
            if snapshot.intent.is_some() {
                assert_eq!(
                    snapshot.state,
                    EntryState::Prepared,
                    "an intent survived on a non-prepared entry"
                );
            }
        }
    }

    // Recovery must converge.
    let _ = cachelito.stale_intents(0);
    for byte in data.iter() {
        let key = format!("k{}", *byte as usize % 8);
        if let Ok(snapshot) = cachelito.peek(key.as_bytes())
            && let Some(intent) = snapshot.intent
        {
            let _ = cachelito.resolve_intent(
                key.as_bytes(),
                intent,
                RecoveryDirection::for_kind(intent.kind),
            );
        }
    }
});
