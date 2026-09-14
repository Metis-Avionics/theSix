//! Backend wiring tests. L4 (sled) and L5 (origin) run fully in-process.
//! L3 (redis) is only tested for construction-free behaviour; live
//! connectivity requires an external server and is not asserted here.
#![cfg(any(feature = "sled", feature = "redis"))]
#![allow(unused_imports)]
mod common;

use std::sync::Arc;
use thesix::{ByteValue, CacheTier, KeyRef};

#[test]
fn test_byte_value_roundtrip() {
    let original = b"hello-backend".to_vec();
    let encoded = ByteValue::encode_bytes(&original).unwrap();
    let decoded = <Vec<u8> as ByteValue>::decode_bytes(&encoded).unwrap();
    assert_eq!(original, decoded);

    let s = "string-value".to_string();
    let bytes = ByteValue::encode_bytes(&s).unwrap();
    let back = <String as ByteValue>::decode_bytes(&bytes).unwrap();
    assert_eq!(s, back);
}

#[cfg(feature = "sled")]
#[test]
fn test_l4_sled_backend_get_set_remove() {
    use thesix::L4SledBackend;

    let dir = std::env::temp_dir().join(format!("thesix-sled-{}", std::process::id()));
    let backend = L4SledBackend::<Vec<u8>>::open(&dir).expect("sled open");

    let key = KeyRef(b"sled-key".as_slice());
    // Miss first.
    assert!(backend.get(&key).unwrap().is_none());
    assert!(!backend.contains(&key).unwrap());

    // Set and read back.
    backend.set(&key, b"sled-value".to_vec(), None).unwrap();
    assert_eq!(backend.get(&key).unwrap(), Some(b"sled-value".to_vec()));
    assert!(backend.contains(&key).unwrap());

    // Remove.
    backend.remove(&key).unwrap();
    assert!(backend.get(&key).unwrap().is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_l5_origin_backend_fetch_through() {
    use thesix::L5OriginBackend;

    let fetcher: Box<thesix::OriginFetcher> = Box::new(|key: &[u8]| {
        if key == b"origin-key" {
            Ok(Some(b"origin-value".to_vec()))
        } else {
            Ok(None)
        }
    });
    let backend = L5OriginBackend::<Vec<u8>>::new(fetcher);

    let key = KeyRef(b"origin-key".as_slice());
    assert_eq!(backend.get(&key).unwrap(), Some(b"origin-value".to_vec()));
    let miss = KeyRef(b"nope".as_slice());
    assert!(backend.get(&miss).unwrap().is_none());
}
