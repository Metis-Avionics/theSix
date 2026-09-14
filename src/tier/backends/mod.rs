//! Value codec for real tier backends.
//!
//! The in-memory tier stubs store `V` directly. Real backends (Redis, sled,
//! origin HTTP) transport and persist **bytes**, so they require a codec to
//! serialize and deserialize `V`. This module provides that codec without
//! changing the core `CacheTier<V>` contract: backends are generic over
//! `V: ByteValue`.
//!
//! A value that is already bytes (`Vec<u8>`) satisfies `ByteValue` with a
//! zero-copy pass-through. Other value types can implement `ByteValue` via a
//! serde-based wrapper of the application's choosing.

use crate::error::CacheError;

#[cfg(feature = "redis")]
pub mod l3_redis;
#[cfg(feature = "sled")]
pub mod l4_sled;
pub mod l5_origin;

#[cfg(feature = "redis")]
pub use l3_redis::L3RedisBackend;
#[cfg(feature = "sled")]
pub use l4_sled::L4SledBackend;
pub use l5_origin::{L5OriginBackend, OriginFetcher, OriginWriter};

/// A value that can be encoded to and decoded from raw bytes for storage in a
/// real backend. Implementations must round-trip: `from_bytes(to_bytes())`
/// must succeed and yield an equivalent value.
pub trait ByteValue: Clone + Send + Sync + 'static {
    /// Encode the value to bytes. `Err` on serialization failure.
    fn encode_bytes(&self) -> Result<Vec<u8>, CacheError>;

    /// Decode a value from bytes previously produced by `encode_bytes`.
    /// `Err` on deserialization failure.
    fn decode_bytes(bytes: &[u8]) -> Result<Self, CacheError>;
}

impl ByteValue for Vec<u8> {
    fn encode_bytes(&self) -> Result<Vec<u8>, CacheError> {
        Ok(self.clone())
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, CacheError> {
        Ok(bytes.to_vec())
    }
}

impl ByteValue for String {
    fn encode_bytes(&self) -> Result<Vec<u8>, CacheError> {
        Ok(self.as_bytes().to_vec())
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, CacheError> {
        String::from_utf8(bytes.to_vec()).map_err(|_| CacheError::SerializationFailed)
    }
}
