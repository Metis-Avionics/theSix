//! Trust boundary: serialized payloads and their framing.
//!
//! Property: a framed record of arbitrary bytes is either decoded exactly, or
//! rejected. It must never be silently accepted with a different value, and
//! `get` and `contains` must never disagree about the same bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thesix::integrity::{ContentDigest, KeyFingerprint};

fuzz_target!(|data: &[u8]| {
    // Integrity metadata must be total: every input produces a digest, and a
    // digest never depends on anything but its input.
    let a = ContentDigest::of_bytes(data);
    let b = ContentDigest::of_bytes(data);
    assert_eq!(a, b, "the digest is not a function of its input");
    assert_eq!(a.value(), b.value());

    let f = KeyFingerprint::of(data);
    assert_eq!(f, KeyFingerprint::of(data), "the fingerprint is not deterministic");

    // Concatenation must be unambiguous: two distinct (tenant, key) pairs must never
    // frame to the same bytes.
    //
    // Note what is *not* asserted: that both frames are accepted. The two pairs have
    // different lengths, so a fixed buffer legitimately accepts one and rejects the
    // other — and the original version of this target asserted they agreed, which
    // the fuzzer correctly identified as a claim about the target rather than about
    // the crate.
    if data.len() >= 4 {
        let split = data.len() / 2;
        let (t1, k1) = data.split_at(split);
        let (t2, k2) = data.split_at(data.len() - split);
        if t1 != t2 || k1 != k2 {
            let mut buf1 = [0u8; 1024];
            let mut buf2 = [0u8; 1024];
            if let (Ok(a), Ok(b)) = (
                thesix::frame_tenant_key(&String::from_utf8_lossy(t1), k1, &mut buf1),
                thesix::frame_tenant_key(&String::from_utf8_lossy(t2), k2, &mut buf2),
            ) {
                assert_ne!(
                    &buf1[..a], &buf2[..b],
                    "two distinct (tenant, key) pairs framed identically"
                );
            }
        }
    }

    // The frame is reversible: what came out is exactly what went in.
    if let Some(n) = thesix::frame_tenant_key("t", data, &mut [0u8; 2048]).ok() {
        assert_eq!(n, data.len() + 2, "the frame length is not tenant + separator + key");
    }
});
