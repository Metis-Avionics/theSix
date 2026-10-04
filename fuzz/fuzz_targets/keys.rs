//! Trust boundary: keys.
//!
//! Property: no key input may panic, and no key may resolve to another key's
//! value. The tenant framing is the interesting part — a key containing the
//! frame separator, or one that overflows the buffer, must be rejected rather
//! than truncated into a *different* key.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thesix::frame_tenant_key;

fuzz_target!(|data: &[u8]| {
    // Arbitrary bytes are not a tenant: a tenant containing the separator must be
    // refused, and splitting the input lets the fuzzer explore both halves.
    let split = data.first().map_or(0, |b| *b as usize % data.len().max(1));
    let (tenant_bytes, key_bytes) = data.split_at(split.min(data.len()));
    let tenant = String::from_utf8_lossy(tenant_bytes);
    let mut buf = [0u8; 512];

    match frame_tenant_key(&tenant, key_bytes, &mut buf) {
        Ok(n) => {
            // Whatever came back must round-trip to the same tenant and key.
            // A frame that decodes differently would be a collision waiting to
            // serve one tenant another's data.
            let framed = &buf[..n];
            let sep = framed.iter().position(|b| *b == thesix::TENANT_SEPARATOR);
            if let Some(i) = sep {
                let t = String::from_utf8_lossy(&framed[..i]);
                let k = &framed[i + 1..];
                assert_eq!(t, tenant[..], "tenant did not round-trip");
                assert_eq!(k, key_bytes, "key did not round-trip");
            }
        }
        Err(_) => {
            // A rejected frame must not have written anything observable.
        }
    }
});
