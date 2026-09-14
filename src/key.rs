use std::hash::Hash;

use crate::error::CacheError;

#[derive(Debug, Clone, Copy)]
pub struct KeyRef<'a>(pub &'a [u8]);

impl PartialEq for KeyRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for KeyRef<'_> {}

impl std::hash::Hash for KeyRef<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<'a> From<&'a [u8]> for KeyRef<'a> {
    fn from(bytes: &'a [u8]) -> Self {
        KeyRef(bytes)
    }
}

impl<'a> From<&'a Vec<u8>> for KeyRef<'a> {
    fn from(bytes: &'a Vec<u8>) -> Self {
        KeyRef(bytes.as_slice())
    }
}

pub trait Key: Hash + Eq + Clone + Send + Sync + std::fmt::Debug {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, CacheError>;
    fn encoded_len(&self) -> usize;
}

impl Key for String {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, CacheError> {
        let bytes = self.as_bytes();
        if buf.len() < bytes.len() {
            return Err(CacheError::ConfigurationError);
        }
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }

    fn encoded_len(&self) -> usize {
        self.as_bytes().len()
    }
}

impl Key for &str {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, CacheError> {
        let bytes = self.as_bytes();
        if buf.len() < bytes.len() {
            return Err(CacheError::ConfigurationError);
        }
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }

    fn encoded_len(&self) -> usize {
        self.as_bytes().len()
    }
}

impl Key for Vec<u8> {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, CacheError> {
        if buf.len() < self.len() {
            return Err(CacheError::ConfigurationError);
        }
        buf[..self.len()].copy_from_slice(self);
        Ok(self.len())
    }

    fn encoded_len(&self) -> usize {
        self.len()
    }
}

impl<K> Key for &K
where
    K: Key,
{
    fn encode(&self, buf: &mut [u8]) -> Result<usize, CacheError> {
        (*self).encode(buf)
    }

    fn encoded_len(&self) -> usize {
        (*self).encoded_len()
    }
}

impl<T> Key for std::sync::Arc<T>
where
    T: Key,
{
    fn encode(&self, buf: &mut [u8]) -> Result<usize, CacheError> {
        self.as_ref().encode(buf)
    }

    fn encoded_len(&self) -> usize {
        self.as_ref().encoded_len()
    }
}
