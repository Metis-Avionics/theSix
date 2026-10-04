use std::hash::Hash;

use crate::error::CacheError;

/// Largest *tenant-framed* key the manager will encode, in bytes.
///
/// The frame is `tenant || 0x00 || application-key`, so this budget covers the
/// tenant as well as the key. It was 256 when the frame covered only the key;
/// raising it is what makes room for a tenant prefix without making ordinary
/// keys fail.
///
/// Public so a caller can check a key's length *before* offering it, rather than
/// discovering the limit as a `ConfigurationError` from `set`.
pub const MAX_KEY_SIZE: usize = 512;

/// Separator between the tenant and the application key inside the frame.
///
/// A zero byte cannot appear in a tenant, because [`crate::IdentityContext`]
/// rejects tenants containing one. That makes the frame unambiguous: without it,
/// tenant `"ab"` + key `"c"` and tenant `"a"` + key `"bc"` would encode to the
/// same bytes and two tenants would share every entry whose keys concatenated
/// alike.
pub const TENANT_SEPARATOR: u8 = 0x00;

/// Build the tenant-framed key.
///
/// Returns `ConfigurationError` when the frame does not fit, rather than
/// truncating: a truncated frame is a *different* key, and silently shortening
/// one tenant's key until it collides with another's is precisely the
/// cross-tenant exposure the framing exists to prevent.
pub fn frame_tenant_key(tenant: &str, key: &[u8], buf: &mut [u8]) -> Result<usize, CacheError> {
    let tenant_bytes = tenant.as_bytes();
    if tenant_bytes.contains(&TENANT_SEPARATOR) {
        return Err(CacheError::ConfigurationError);
    }
    let needed = tenant_bytes
        .len()
        .checked_add(1)
        .and_then(|n| n.checked_add(key.len()))
        .ok_or(CacheError::ConfigurationError)?;
    if needed > buf.len() {
        return Err(CacheError::ConfigurationError);
    }
    let (prefix, rest) = buf.split_at_mut(tenant_bytes.len());
    prefix.copy_from_slice(tenant_bytes);
    rest[0] = TENANT_SEPARATOR;
    rest[1..=key.len()].copy_from_slice(key);
    Ok(needed)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_separates_tenants_that_would_otherwise_concatenate_alike() {
        // The whole reason a separator exists: without one these are the same
        // bytes, so two tenants would share every entry.
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        let la = frame_tenant_key("ab", b"c", &mut a).expect("frame");
        let lb = frame_tenant_key("a", b"bc", &mut b).expect("frame");
        assert_ne!(&a[..la], &b[..lb], "tenant framing is ambiguous");
    }

    #[test]
    fn the_same_tenant_and_key_frame_identically() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        let la = frame_tenant_key("t", b"k", &mut a).expect("frame");
        let lb = frame_tenant_key("t", b"k", &mut b).expect("frame");
        assert_eq!(&a[..la], &b[..lb]);
    }

    #[test]
    fn a_tenant_containing_the_separator_is_rejected() {
        let mut buf = [0u8; 64];
        assert_eq!(
            frame_tenant_key("a\0b", b"k", &mut buf),
            Err(CacheError::ConfigurationError)
        );
    }

    #[test]
    fn an_oversized_frame_is_rejected_rather_than_truncated() {
        let mut buf = [0u8; 8];
        assert_eq!(
            frame_tenant_key("tenant", b"key", &mut buf),
            Err(CacheError::ConfigurationError),
            "a frame that does not fit must not be shortened, because a shortened \
             frame is a different key"
        );
    }

    #[test]
    fn an_empty_tenant_still_frames_deterministically() {
        let mut a = [0u8; 16];
        let n = frame_tenant_key("", b"k", &mut a).expect("frame");
        assert_eq!(&a[..n], &[0u8, b'k']);
    }
}
