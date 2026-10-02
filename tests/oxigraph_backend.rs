//! The Oxigraph RDF/SPO backend.
//!
//! Runs only with the `oxigraph` feature, needs no external service, and is
//! therefore genuinely verified rather than merely compiled.

#![cfg(feature = "oxigraph")]

use thesix::tier::backends::L5OxigraphBackend;
use thesix::{ByteValue, CacheTier, KeyRef, TierId};

fn key(s: &str) -> KeyRef<'_> {
    KeyRef::from(s.as_bytes())
}

#[tokio::test]
async fn round_trips_a_value() {
    let tier: L5OxigraphBackend<Vec<u8>> = L5OxigraphBackend::new().expect("store");

    assert_eq!(tier.backend(), thesix::BackendKind::Oxigraph);
    assert_eq!(tier.tier_id(), TierId::L5);
    assert_eq!(tier.name(), "L5-oxigraph");

    assert!(tier.get(&key("absent")).await.expect("get").is_none());
    assert!(!tier.contains(&key("absent")).await.expect("contains"));

    tier.set(&key("k"), b"payload".to_vec(), None)
        .await
        .expect("set");
    assert_eq!(
        tier.get(&key("k")).await.expect("get"),
        Some(b"payload".to_vec())
    );
    assert!(tier.contains(&key("k")).await.expect("contains"));

    tier.remove(&key("k")).await.expect("remove");
    assert!(tier.get(&key("k")).await.expect("get").is_none());
}

/// Overwriting must replace, not accumulate. RDF stores happily hold many quads
/// for one subject, so a naive insert would leave every historical value
/// readable and make `get` return whichever the store yielded first.
#[tokio::test]
async fn overwrite_replaces_rather_than_accumulates() {
    let tier: L5OxigraphBackend<Vec<u8>> = L5OxigraphBackend::new().expect("store");

    tier.set(&key("k"), b"first".to_vec(), None)
        .await
        .expect("set");
    tier.set(&key("k"), b"second".to_vec(), None)
        .await
        .expect("set");

    assert_eq!(
        tier.get(&key("k")).await.expect("get"),
        Some(b"second".to_vec()),
        "a re-set must be visible, not shadowed by an older quad"
    );

    // And removing clears every quad for the subject, not just the first.
    tier.remove(&key("k")).await.expect("remove");
    assert!(tier.get(&key("k")).await.expect("get").is_none());
}

/// Keys are arbitrary bytes but RDF subjects must be IRIs. This exercises the
/// encoding on inputs that would break a naive string interpolation: spaces,
/// newlines, non-UTF-8, and the empty key.
#[tokio::test]
async fn arbitrary_byte_keys_encode_into_valid_iris() {
    let tier: L5OxigraphBackend<Vec<u8>> = L5OxigraphBackend::new().expect("store");

    let keys: Vec<Vec<u8>> = vec![
        b"plain".to_vec(),
        b"with space".to_vec(),
        b"with\nnewline".to_vec(),
        b"<not-an-iri>".to_vec(),
        vec![0xff, 0xfe, 0x00, 0x01],
        Vec::new(),
    ];

    for raw in &keys {
        let kr = KeyRef::from(raw.as_slice());
        tier.set(&kr, b"v".to_vec(), None)
            .await
            .unwrap_or_else(|e| panic!("key {raw:?} failed to encode: {e:?}"));
        assert_eq!(
            tier.get(&kr).await.expect("get"),
            Some(b"v".to_vec()),
            "key {raw:?} did not round-trip"
        );
    }

    // Distinct keys must not collide after encoding. These must genuinely
    // differ: an earlier draft compared b"ab" against b"a\x62", which are the
    // same two bytes, so it asserted a collision that was not a collision.
    let pairs: [(&[u8], &[u8]); 4] = [
        (b"ab", b"aB"),
        (b"ab", b"abc"),
        (b"1", b"01"),
        (b"ff", b"\xff"),
    ];
    for (left, right) in pairs {
        assert_ne!(left, right, "test inputs must differ");
        tier.set(&KeyRef::from(left), b"first".to_vec(), None)
            .await
            .expect("set");
        tier.set(&KeyRef::from(right), b"second".to_vec(), None)
            .await
            .expect("set");
        assert_eq!(
            tier.get(&KeyRef::from(left)).await.expect("get"),
            Some(b"first".to_vec()),
            "keys {left:?} and {right:?} collided after encoding"
        );
        assert_eq!(
            tier.get(&KeyRef::from(right)).await.expect("get"),
            Some(b"second".to_vec()),
            "keys {left:?} and {right:?} collided after encoding"
        );
    }
}

/// Non-UTF-8 payloads must survive the literal round-trip, since `ByteValue` is
/// defined over bytes and a lossy conversion would corrupt them silently.
#[tokio::test]
async fn non_utf8_payloads_round_trip_losslessly() {
    let tier: L5OxigraphBackend<Vec<u8>> = L5OxigraphBackend::new().expect("store");
    let payload = vec![0x00u8, 0xff, 0xfe, 0x80, b'a'];

    tier.set(&key("bin"), payload.clone(), None)
        .await
        .expect("set");
    assert_eq!(
        tier.get(&key("bin")).await.expect("get"),
        Some(payload),
        "a non-UTF-8 payload was corrupted by the literal round-trip"
    );
}

/// `Vec<u8>` is the zero-copy `ByteValue`, which is what makes this backend
/// usable without the caller writing a codec.
#[tokio::test]
async fn byte_value_passthrough_is_used() {
    let tier: L5OxigraphBackend<Vec<u8>> = L5OxigraphBackend::new().expect("store");
    let payload = b"zero-copy".to_vec();
    assert_eq!(
        Vec::<u8>::decode_bytes(&payload).expect("pass-through decode"),
        payload
    );
    tier.set(&key("z"), payload.clone(), None)
        .await
        .expect("set");
    assert_eq!(tier.get(&key("z")).await.expect("get"), Some(payload));
}
