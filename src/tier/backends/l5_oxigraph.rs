//! An RDF/SPO backend for the search tier, on `oxigraph`.
//!
//! Each cache entry becomes one quad: the entry key is the subject IRI, a fixed
//! predicate names the slot, and the value is a literal. That mapping is what
//! makes the tier a *source* rather than a plain key/value store - a consumer
//! can query across entries with SPARQL instead of only looking keys up.
//!
//! # Key encoding
//!
//! `KeyRef` is an arbitrary byte string and RDF subjects must be IRIs, so keys
//! are hex-encoded into a namespaced IRI. Hex rather than base64 because IRI
//! grammars forbid several base64 characters, and hex keeps the mapping
//! obviously reversible when someone reads a store with an external tool.
//!
//! # Storage scope
//!
//! Built on an in-process `oxigraph::store::Store`. That is deliberate and is
//! the reason `default-features = false` matters: oxigraph's default feature is
//! `rocksdb`, which would pull a C++ toolchain into a stock-Rust build for a
//! backend nobody has asked to make durable yet. `L4SledBackend` is the
//! persistent local rung; this one is an RDF projection source.

use oxigraph::model::{GraphNameRef, Literal, NamedNode, NamedOrBlankNode, Quad, QuadRef, Term};
use oxigraph::store::Store;

use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::TierId;
use crate::tier::backends::ByteValue;
use crate::tier::tier_trait::{BackendKind, CacheTier, TierHealth};

/// Namespace for entry subjects. Changing it would orphan every existing entry,
/// so it is a constant rather than a constructor argument.
const ENTRY_NS: &str = "urn:thesix:entry:";
/// Predicate marking the value slot of an entry.
const VALUE_PRED: &str = "urn:thesix:value";

/// Hex-encodes bytes for embedding in an IRI or a literal.
///
/// Lossless, which matters: an earlier version stored the value through
/// `String::from_utf8_lossy`, and that silently replaced every invalid byte with
/// U+FFFD. A binary payload came back altered and nothing reported an error,
/// which is worse than a failure. `ByteValue` is defined over bytes, so the
/// transport has to be too.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn hex_decode(text: &str) -> Result<Vec<u8>, CacheError> {
    if !text.len().is_multiple_of(2) {
        return Err(CacheError::SerializationFailed);
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char)
            .to_digit(16)
            .ok_or(CacheError::SerializationFailed)?;
        let lo = (pair[1] as char)
            .to_digit(16)
            .ok_or(CacheError::SerializationFailed)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

/// An L5 tier backed by an in-process RDF store.
pub struct L5OxigraphBackend<V> {
    store: Store,
    _marker: std::marker::PhantomData<V>,
}

impl<V> std::fmt::Debug for L5OxigraphBackend<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L5OxigraphBackend").finish_non_exhaustive()
    }
}

impl<V: ByteValue> L5OxigraphBackend<V> {
    /// Build an empty in-process store.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the store cannot be created, which is an allocation
    /// failure at construction time.
    pub fn new() -> Result<Self, CacheError> {
        Ok(Self {
            store: Store::new().map_err(map_store_error)?,
            _marker: std::marker::PhantomData,
        })
    }

    /// Builds the entry subject IRI for a key.
    ///
    /// Returns `Err` rather than unwrapping: the input is hex plus a constant
    /// namespace and so is always a valid IRI, but "always" is a claim about a
    /// format this function does not own, and the crate denies `expect` on this
    /// path. Propagating keeps that claim testable instead of asserted.
    fn subject_for(key: &KeyRef<'_>) -> Result<NamedNode, CacheError> {
        let mut iri = String::with_capacity(key.0.len() * 2 + ENTRY_NS.len());
        iri.push_str(ENTRY_NS);
        iri.push_str(&hex_encode(key.0));
        NamedNode::new(iri).map_err(map_store_error)
    }

    fn value_predicate() -> Result<NamedNode, CacheError> {
        NamedNode::new(VALUE_PRED).map_err(map_store_error)
    }

    fn find_value(&self, key: &KeyRef<'_>) -> Result<Option<Vec<u8>>, CacheError> {
        let subject = Self::subject_for(key)?;
        let pattern_subject: NamedOrBlankNode = subject.clone().into();
        for found in self
            .store
            .quads_for_pattern(Some(pattern_subject.as_ref()), None, None, None)
        {
            let q = found.map_err(map_store_error)?;
            if let NamedOrBlankNode::NamedNode(node) = &q.subject
                && node.as_str() == subject.as_str()
            {
                return Ok(Some(match &q.object {
                    Term::Literal(l) => l.value().as_bytes().to_vec(),
                    _ => Vec::new(),
                }));
            }
        }
        Ok(None)
    }
}

/// oxigraph surfaces storage problems as its own error type; the cache only
/// distinguishes "that failed" from "that succeeded", and the distinction a
/// caller can act on is retryable-or-not. A store-level failure is not.
fn map_store_error(e: impl std::fmt::Display) -> CacheError {
    let _ = e;
    CacheError::PopulationFailed
}

#[async_trait::async_trait]
impl<V: ByteValue> CacheTier<V> for L5OxigraphBackend<V> {
    fn name(&self) -> String {
        "L5-oxigraph".into()
    }

    fn backend(&self) -> BackendKind {
        BackendKind::Oxigraph
    }

    fn capability(&self) -> crate::capability::TierCapability {
        // The in-process `Store` is not a durable store, whatever this rung is
        // meant to become. Claiming persistence here is exactly the false
        // durability claim the contract forbids, so it is `Volatile` until a
        // real backend replaces it.
        crate::capability::TierCapability::new(
            BackendKind::Oxigraph,
            crate::capability::CapabilityFlags::BLOCKING_IO,
            crate::capability::OperationalState::Healthy,
            crate::capability::DurabilityClass::Volatile,
        )
    }

    async fn get(&self, key: &KeyRef<'_>) -> Result<Option<V>, CacheError> {
        match self.find_value(key)? {
            Some(encoded) => {
                let text =
                    std::str::from_utf8(&encoded).map_err(|_| CacheError::SerializationFailed)?;
                V::decode_bytes(&hex_decode(text)?).map(Some)
            }
            None => Ok(None),
        }
    }

    async fn set(
        &self,
        key: &KeyRef<'_>,
        value: V,
        _ttl: Option<std::time::Duration>,
    ) -> Result<(), CacheError> {
        let bytes = value.encode_bytes()?;
        let subject = Self::subject_for(key)?;
        let predicate = Self::value_predicate()?;
        let object = Literal::new_simple_literal(hex_encode(&bytes));

        // Store takes borrowed refs, so the parts are bound first and the quad
        // is assembled from them. Replace-then-insert: re-setting a key must not
        // accumulate quads for the same subject.
        let quad = QuadRef::new(
            subject.as_ref(),
            predicate.as_ref(),
            object.as_ref(),
            GraphNameRef::DefaultGraph,
        );
        self.store.remove(quad).map_err(map_store_error)?;
        self.store.insert(quad).map_err(map_store_error)?;
        Ok(())
    }

    async fn remove(&self, key: &KeyRef<'_>) -> Result<(), CacheError> {
        let subject = Self::subject_for(key)?;
        let pattern_subject: NamedOrBlankNode = subject.into();
        // The iterator itself is infallible; individual items may not be. A quad
        // that cannot be read back cannot be removed either, so surface it
        // rather than silently skipping and leaving the entry behind.
        let mut to_remove: Vec<Quad> = Vec::new();
        for found in self
            .store
            .quads_for_pattern(Some(pattern_subject.as_ref()), None, None, None)
        {
            to_remove.push(found.map_err(map_store_error)?);
        }
        for q in &to_remove {
            // `remove` borrows: `From<&Quad> for QuadRef` is the conversion.
            self.store.remove(q).map_err(map_store_error)?;
        }
        Ok(())
    }

    async fn contains(&self, key: &KeyRef<'_>) -> Result<bool, CacheError> {
        Ok(self.find_value(key)?.is_some())
    }

    fn health(&self) -> TierHealth {
        TierHealth::default()
    }

    fn tier_id(&self) -> TierId {
        TierId::L5
    }
}
