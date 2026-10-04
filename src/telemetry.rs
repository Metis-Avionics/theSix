//! Structured operation records.
//!
//! Enough state to reconstruct any single operation after the fact: what was
//! asked, which rung answered, which one it came from or went to, how long it
//! took, and how it ended.
//!
//! # What is deliberately absent
//!
//! **Payloads.** Never. A record carries a [`KeyIdentity`], not a key, and never
//! a value. `cia.confidentiality.payload_in_telemetry = false` is only meaningful
//! if the type makes it impossible to put one in, so there is no field to fill
//! with one.
//!
//! **Keys.** [`KeyIdentity`] is a non-reversible digest plus the key's length.
//! Keys are as sensitive as the data they name — a key is often a tenant id, a
//! user id, or a URL — so logging the bytes would leak exactly what the
//! confidentiality clauses are protecting. The length is kept because "this key
//! is 40 bytes and that one is 3" is diagnostically useful and reveals nothing.
//!
//! No dependency: [`TelemetrySink`] is a plain trait, so a consumer can bridge
//! to whatever it already uses without theSix choosing a tracing framework.

use std::sync::Arc;
use std::time::Duration;

use crate::entry::Generation;
use crate::error::CacheError;
use crate::key::KeyRef;
use crate::tier::TierId;
use crate::tier::tier_trait::BackendKind;

/// A key, identified without revealing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyIdentity {
    /// FNV-1a over the key bytes. Not cryptographic and not reversible; a digest
    /// collision merges two keys' records, which is the safe direction to fail.
    digest: u64,
    /// Key length in bytes.
    len: u16,
}

impl KeyIdentity {
    #[must_use]
    pub fn of(key: &[u8]) -> Self {
        Self {
            digest: fnv1a64(key),
            len: u16::try_from(key.len()).unwrap_or(u16::MAX),
        }
    }

    #[must_use]
    pub const fn digest(self) -> u64 {
        self.digest
    }

    /// The key's length in bytes.
    #[must_use]
    pub const fn len(self) -> u16 {
        self.len
    }

    /// Whether the key was empty. Present because a public `len` without it is a
    /// `clippy::len_without_is_empty` hazard, and an empty key is a real input.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl std::fmt::Display for KeyIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hex digest and length. Deliberately not the key.
        write!(f, "k{:016x}/{}", self.digest, self.len)
    }
}

/// FNV-1a, 64-bit. Chosen because it is three lines, allocation-free, and
/// deterministic across platforms — which `DefaultHasher` is explicitly not, and
/// which matters because these digests end up in persisted logs.
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// What kind of operation a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    Get,
    GetOrFetch,
    Set,
    Invalidate,
    Remove,
    Exists,
    Refresh,
    Promote,
    Demote,
    Recover,
}

impl Operation {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::GetOrFetch => "get_or_fetch",
            Self::Set => "set",
            Self::Invalidate => "invalidate",
            Self::Remove => "remove",
            Self::Exists => "exists",
            Self::Refresh => "refresh",
            Self::Promote => "promote",
            Self::Demote => "demote",
            Self::Recover => "recover",
        }
    }
}

/// Why an operation ended the way it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    Served,
    Miss,
    Populated,
    /// Served from a lower rung than the one that was asked for.
    FellBack,
    /// Rejected before touching data, by authn or authz.
    Denied,
    Failed,
    Cancelled,
}

impl Outcome {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Served => "served",
            Self::Miss => "miss",
            Self::Populated => "populated",
            Self::FellBack => "fell_back",
            Self::Denied => "denied",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub const fn from_error(e: CacheError) -> Self {
        match e {
            CacheError::Unauthenticated | CacheError::Unauthorized | CacheError::PolicyDenied => {
                Self::Denied
            }
            CacheError::Cancelled => Self::Cancelled,
            CacheError::Miss => Self::Miss,
            _ => Self::Failed,
        }
    }
}

/// The eleven fields the contract's `[observability.operation]` requires.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OperationRecord {
    /// What was attempted.
    pub operation: Operation,
    /// The key, identified by digest.
    pub key: KeyIdentity,
    /// Rung the value came from, when the operation read one.
    pub source_tier: Option<TierId>,
    /// Rung the value went to, when the operation wrote one.
    pub destination_tier: Option<TierId>,
    /// What the answering rung is bound to.
    pub backend: Option<BackendKind>,
    /// Control-plane generation the operation observed.
    pub generation: Option<Generation>,
    /// Wall time for the whole operation.
    pub latency: Duration,
    /// How it ended.
    pub outcome: Outcome,
    /// Whether a fallback was involved.
    pub fallback: bool,
    /// The error, when there was one. Carries no payload: `CacheError` has no
    /// payload field to leak through.
    pub failure_class: Option<CacheError>,
    /// Continuity state of the answering rung after the operation.
    pub recovery_state: Option<crate::continuity::ContinuityState>,
}

impl OperationRecord {
    /// Start a record. Call [`Self::finish`] to complete it.
    #[must_use]
    pub fn begin(operation: Operation, key: &KeyRef<'_>) -> Self {
        Self {
            operation,
            key: KeyIdentity::of(key.0),
            source_tier: None,
            destination_tier: None,
            backend: None,
            generation: None,
            latency: Duration::ZERO,
            outcome: Outcome::Served,
            fallback: false,
            failure_class: None,
            recovery_state: None,
        }
    }

    #[must_use]
    pub fn finish(mut self, started: std::time::Instant, outcome: Outcome) -> Self {
        self.latency = started.elapsed();
        self.outcome = outcome;
        self
    }

    #[must_use]
    pub fn with_source(mut self, tier: TierId) -> Self {
        self.source_tier = Some(tier);
        self
    }

    #[must_use]
    pub fn with_destination(mut self, tier: TierId) -> Self {
        self.destination_tier = Some(tier);
        self
    }

    #[must_use]
    pub fn with_backend(mut self, backend: BackendKind) -> Self {
        self.backend = Some(backend);
        self
    }

    #[must_use]
    pub fn with_generation(mut self, generation: Generation) -> Self {
        self.generation = Some(generation);
        self
    }

    #[must_use]
    pub fn with_fallback(mut self, fallback: bool) -> Self {
        self.fallback = fallback;
        self
    }

    #[must_use]
    pub fn with_failure(mut self, e: CacheError) -> Self {
        self.failure_class = Some(e);
        self.outcome = Outcome::from_error(e);
        self
    }

    #[must_use]
    pub fn with_recovery_state(mut self, state: crate::continuity::ContinuityState) -> Self {
        self.recovery_state = Some(state);
        self
    }
}

impl std::fmt::Display for OperationRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "op={} key={} src={:?} dst={:?} backend={:?} gen={} latency_us={} outcome={} fallback={} failure={:?} recovery={:?}",
            self.operation.name(),
            self.key,
            self.source_tier.map(|t| t.to_string()),
            self.destination_tier.map(|t| t.to_string()),
            self.backend.map(|b| b.to_string()),
            self.generation
                .map_or_else(|| "-".to_string(), |g| g.to_string()),
            self.latency.as_micros(),
            self.outcome.name(),
            self.fallback,
            self.failure_class,
            self.recovery_state.map(|s| s.to_string()),
        )
    }
}

/// Where records go.
///
/// `Send + Sync` and infallible on purpose: telemetry must not be able to fail an
/// operation. A sink that panics or returns an error would make observability a
/// correctness hazard, which is the opposite of what it is for.
pub trait TelemetrySink: Send + Sync {
    fn record(&self, record: &OperationRecord);
}

/// Discards everything. The default, so an application that does not want
/// telemetry does not allocate a sink or pay a branch it cannot use.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTelemetry;

impl TelemetrySink for NoTelemetry {
    fn record(&self, _record: &OperationRecord) {}
}

/// Keeps the last `n` records in memory. For tests and for a consumer that wants
/// to assert on its own observability rather than trust it.
#[derive(Debug)]
pub struct RingTelemetry {
    buf: std::sync::Mutex<std::collections::VecDeque<OperationRecord>>,
    cap: usize,
}

impl RingTelemetry {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            // A zero-capacity ring would drop everything, which is a silent
            // no-op sink. Clamp to 1 so the type always retains something.
            buf: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(cap.max(1))),
            cap: cap.max(1),
        }
    }

    /// The retained records, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<OperationRecord> {
        self.buf
            .lock()
            .map(|b| b.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Records whose key digest matches. Used by tests to prove a specific
    /// operation was observed without exposing the key.
    #[must_use]
    pub fn records_for(&self, key: &[u8]) -> Vec<OperationRecord> {
        let id = KeyIdentity::of(key);
        self.records().into_iter().filter(|r| r.key == id).collect()
    }

    pub fn len(&self) -> usize {
        self.buf.lock().map_or(0, |b| b.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Latency percentiles over the retained records.
    ///
    /// A hand-rolled sort rather than a statistics dependency: the contract asks
    /// for p50/p95/p99/p99.9/max and a general stats crate would be a large
    /// dependency for five numbers computed over at most a few thousand samples.
    #[must_use]
    pub fn percentiles(&self) -> LatencyPercentiles {
        let mut samples: Vec<u64> = self
            .records()
            .iter()
            .map(|r| r.latency.as_micros() as u64)
            .collect();
        LatencyPercentiles::from_samples(&mut samples)
    }
}

impl TelemetrySink for RingTelemetry {
    fn record(&self, record: &OperationRecord) {
        if let Ok(mut buf) = self.buf.lock() {
            if buf.len() == self.cap {
                buf.pop_front();
            }
            buf.push_back(*record);
        }
        // A poisoned telemetry lock must not fail the operation being measured.
    }
}

/// The latency measures the contract's `[hpa.performance]` requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LatencyPercentiles {
    pub count: usize,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub p99_9: u64,
    pub max: u64,
}

impl LatencyPercentiles {
    /// Nearest-rank percentiles, computed entirely in integer arithmetic.
    ///
    /// The obvious implementation multiplies a `f64` quantile by a length and
    /// casts, which is both a sign-losing cast and a place for the nearest-rank
    /// rule to be subtly wrong. Taking the quantile as a fraction with an explicit
    /// denominator keeps every step integral: rank = ceil(num * len / den),
    /// clamped to `1..=len`, then index `rank - 1`.
    #[must_use]
    pub fn from_samples(samples: &mut [u64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_unstable();
        let len = samples.len() as u128;
        let at = |num: u128, den: u128| -> u64 {
            let rank = (num * len).div_ceil(den).clamp(1, len);
            samples[rank as usize - 1]
        };
        Self {
            count: samples.len(),
            p50: at(50, 100),
            p95: at(95, 100),
            p99: at(99, 100),
            p99_9: at(999, 1000),
            max: *samples.last().unwrap_or(&0),
        }
    }
}

impl std::fmt::Display for LatencyPercentiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} p50={}us p95={}us p99={}us p99.9={}us max={}us",
            self.count, self.p50, self.p95, self.p99, self.p99_9, self.max
        )
    }
}

/// A shared sink handle. Cloning is cheap, so a `CacheManager` can hold one and
/// hand it to every operation.
pub type SharedSink = Arc<dyn TelemetrySink>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_identity_does_not_contain_the_key() {
        let key = b"tenant-7/user-42/secret-orders";
        let id = KeyIdentity::of(key);
        let shown = id.to_string();
        assert!(!shown.contains("tenant-7"));
        assert!(!shown.contains("secret-orders"));
        assert!(shown.starts_with('k'));
        assert_eq!(id.len() as usize, key.len());
    }

    #[test]
    fn distinct_keys_get_distinct_identities() {
        let a = KeyIdentity::of(b"alpha");
        let b = KeyIdentity::of(b"beta");
        assert_ne!(a, b);
        assert_eq!(a, KeyIdentity::of(b"alpha"), "must be deterministic");
    }

    #[test]
    fn fnv1a_is_the_published_vector() {
        // Standard FNV-1a 64 test vector. If this changes, every previously
        // recorded digest stops matching and cross-run correlation breaks
        // silently.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn ring_retains_the_most_recent() {
        let ring = RingTelemetry::new(2);
        for i in 0..3_u64 {
            let rec = OperationRecord::begin(Operation::Get, &KeyRef(b"k"))
                .finish(std::time::Instant::now(), Outcome::Miss);
            let mut rec = rec;
            rec.generation = Some(Generation::new(i));
            ring.record(&rec);
        }
        assert_eq!(ring.len(), 2);
        let kept = ring.records();
        assert_eq!(kept[0].generation, Some(Generation::new(1)));
        assert_eq!(kept[1].generation, Some(Generation::new(2)));
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut s: Vec<u64> = (1..=100).collect();
        let p = LatencyPercentiles::from_samples(&mut s);
        assert_eq!(p.count, 100);
        assert_eq!(p.p50, 50);
        assert_eq!(p.p95, 95);
        assert_eq!(p.p99, 99);
        assert_eq!(p.max, 100);
    }

    #[test]
    fn empty_percentiles_are_zero_not_a_panic() {
        let p = LatencyPercentiles::from_samples(&mut []);
        assert_eq!(p.count, 0);
        assert_eq!(p.max, 0);
    }

    #[test]
    fn records_for_selects_by_key_without_revealing_it() {
        let ring = RingTelemetry::new(8);
        ring.record(&OperationRecord::begin(Operation::Set, &KeyRef(b"alpha")));
        ring.record(&OperationRecord::begin(Operation::Get, &KeyRef(b"beta")));
        assert_eq!(ring.records_for(b"alpha").len(), 1);
        assert_eq!(ring.records_for(b"alpha")[0].operation, Operation::Set);
        assert_eq!(ring.records_for(b"gamma").len(), 0);
    }

    #[test]
    fn every_contract_field_is_present_on_the_record() {
        // Structurally guaranteed, but asserted so adding a field without
        // deciding what it means to the contract shows up as a failing test
        // rather than as a silently incomplete log line.
        let rec = OperationRecord::begin(Operation::Set, &KeyRef(b"k"))
            .with_source(TierId::L1)
            .with_destination(TierId::L1)
            .with_backend(BackendKind::InMemory)
            .with_generation(Generation::new(7))
            .with_fallback(false)
            .with_recovery_state(crate::continuity::ContinuityState::Healthy);
        let rendered = rec.to_string();
        for field in [
            "op=",
            "key=",
            "src=",
            "dst=",
            "backend=",
            "gen=",
            "latency_us=",
            "outcome=",
            "fallback=",
            "failure=",
            "recovery=",
        ] {
            assert!(
                rendered.contains(field),
                "record is missing {field}: {rendered}"
            );
        }
    }
}
