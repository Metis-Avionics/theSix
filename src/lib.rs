#![deny(warnings)]
#![forbid(unsafe_code)]
#![warn(
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
#![allow(
    clippy::unused_async,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    clippy::new_without_default,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::unnecessary_wraps,
    clippy::unused_self,
    clippy::ignored_unit_patterns,
    clippy::needless_continue,
    clippy::match_same_arms,
    clippy::needless_as_bytes
)]
// This lint name differs across clippy versions; allow it without failing on
// toolchains that pre-date it (CI pins an older clippy than some local builds).
#![allow(unknown_lints, clippy::unused_async_trait_impl)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! # theSix
//!
//! Policy-driven six-tier cache orchestration, built around an explicit
//! **high-performance availability (HPA) data-continuity contract**.
//!
//! ```text
//!                     Consumer
//!                         |
//!                         v
//!         theSix continuity / policy plane
//!         locality | availability | performance
//!         consistency | durability | recovery
//!                         |
//!     +-------------------+-------------------+
//!     v                   v                   v
//! Performance         Continuity           Security
//! & locality          & recovery           CIA
//!     +-------------------+-------------------+
//!                         v
//!         Heterogeneous storage / cache / authority
//! ```
//!
//! Consumers depend on the *continuity contract*, not on a backend. L0-L6 are
//! replaceable infrastructure; the rung a value lands on is policy's decision,
//! and authority is configured rather than inferred from a tier number.
//!
//! # Guarantees
//!
//! | Property | Mechanism |
//! |---|---|
//! | Atomicity | Two-phase commit with a payload-free intent record. A crash leaves an intent, never a half-visible value. |
//! | Isolation | Single-flight population ownership; no control guard is ever held across an `.await`. |
//! | Durability honesty | `DurabilityClass` is `Volatile`, `Delegated` or `Verified`. Only a restart test may grant `Verified`. |
//! | Integrity | Every stored value carries a content digest; a damaged record is refused, not served. |
//! | Tenant isolation | The tenant is framed into the key, so a cross-tenant read finds nothing. |
//! | Capability honesty | A rung is reported as `Unbound` rather than silently replaced by another. |
//! | Bounded control plane | Every rung-scanning path is bounded by `LAST_CACHE_TIER`, so a fallback can never surface authority data. |
//!
//! # The control plane and the data plane
//!
//! [`Cachelito`] is the control-plane state registry: a pre-allocated sharded
//! slot map holding entry state, generation, population ownership and a commit
//! intent. It stores **no application payloads** — the intent record is a key
//! hash, a target rung, a generation and a kind, so a crash cannot leak a value
//! through it.
//!
//! [`CacheTier`] implementations are the data plane: they store and retrieve
//! values, and decide nothing. They are `async` because real backends are I/O.
//!
//! [`CacheManager`] sits between them, enforcing authentication, delegating tier
//! selection to a [`CachePolicy`], and coordinating through [`Cachelito`]. Every
//! control-plane call is synchronous and returns an owned snapshot, so
//! no shard guard can be held across an `.await` by construction.
//!
//! # Atomicity, concretely
//!
//! ```text
//! prepare(key, generation, rung)  ->  entry is Prepared; reads see a miss
//! write to the rung               ->  no payload anywhere in the control plane
//! commit(token)                   ->  Ready, intent cleared, waiters notified
//! abort(token)                    ->  restores the interrupted state
//! ```
//!
//! Recovery resolves an outstanding intent by kind: a [`IntentKind::Write`]
//! aborts, because the value is reproducible; an [`IntentKind::Move`] completes
//! forward, because aborting it would discard an already-committed value. Both
//! directions are idempotent.
//!
//! # Capability semantics
//!
//! Asking "which backend is this?" previously had no honest answer — the only
//! way to find out was to issue an operation and receive `TierUnavailable`, which
//! cannot distinguish *not compiled in*, *bound but down*, and *never
//! implemented*. See [`TierCapability`], [`CapabilityFlags`],
//! [`OperationalState`] and [`DurabilityClass`].
//!
//! Those are three axes rather than one enum because the properties are not
//! mutually exclusive: a rung can be persistent *and* shared *and* degraded at
//! the same moment.
//!
//! # Observability
//!
//! [`OperationRecord`] carries the eleven fields needed to reconstruct an
//! operation, with two deliberate omissions: no payload, and no key. A key is
//! identified by a non-reversible [`KeyIdentity`] — a digest plus a length —
//! because keys are as sensitive as the data they name.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use thesix::{CacheContext, CacheManager, CacheTier, Cachelito, DefaultPolicy,
//!              IdentityContext, L0Stub, L1Stub, L2Stub, L3Stub, L4Stub, L5Stub,
//!              MemoryPool, TierRegistry};
//!
//! # async fn example() {
//! let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
//!     Arc::new(L0Stub::<String>::new()),
//!     Arc::new(L1Stub::<String>::new()),
//!     Arc::new(L2Stub::<String>::new()),
//!     Arc::new(L3Stub::<String>::new()),
//!     Arc::new(L4Stub::<String>::new()),
//!     Arc::new(L5Stub::<String>::new()),
//! ];
//! let pool = MemoryPool::<String>::new(1024).expect("pool");
//!
//! let manager: CacheManager<String, String, DefaultPolicy> =
//!     CacheManager::new(DefaultPolicy, Cachelito::new(), TierRegistry::new(), tiers, pool);
//!
//! // Identity carries the tenant, and the tenant is part of the key: two tenants
//! // using the same application key never share an entry.
//! let ctx = CacheContext::new(IdentityContext::new(
//!     "alice".to_string(),
//!     vec!["reader".to_string()],
//!     "tenant-1".to_string(),
//! ));
//!
//! // Application code never selects a tier.
//! let value = manager
//!     .get_or_fetch(&"order-42".to_string(), &ctx, || async { Ok("payload".to_string()) })
//!     .await;
//! # }
//! ```
//!
//! # Crate layout
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`manager`] | `CacheManager` — the public API and orchestration |
//! | [`control`] | `Cachelito` — control-plane state, generations, commit intents |
//! | [`policy`] | `CachePolicy`, operations, decisions, the ladder bound |
//! | [`capability`] | `TierCapability` and its three reporting axes |
//! | [`continuity`] | Continuity states, recovery direction and outcomes |
//! | [`integrity`] | Content digests, key fingerprints, slot placement |
//! | [`telemetry`] | `OperationRecord`, `TelemetrySink`, latency percentiles |
//! | [`tier`] | The `CacheTier` trait and the L0-L6 bindings |
//! | [`entry`] | `EntryState`, `Generation`, `CommitToken` |
//! | [`error`] | `CacheError` — the single error type |
//! | [`identity`] | `IdentityContext` and `CacheContext` |
//! | [`key`] | `Key`, `KeyRef`, tenant key framing |
//! | [`pool`] | `MemoryPool`, a fixed-capacity value allocator |
//! | [`fault`] | Deterministic, seedable fault injection (feature `faults`) |
//!
//! # Contract
//!
//! [`theSix.toml`](./theSix.toml) is the source of truth for the architecture and
//! is machine-checked: `cargo xtask contract` validates it, and `tests/contract`
//! asserts it agrees with this crate.

pub mod capability;
pub mod continuity;
pub mod control;
pub mod entry;
pub mod error;
#[cfg(feature = "faults")]
pub mod fault;
pub mod identity;
pub mod integrity;
pub mod key;
pub mod manager;
pub mod policy;
pub mod pool;
pub mod telemetry;
pub mod tier;

pub use capability::{CapabilityFlags, DurabilityClass, OperationalState, TierCapability};
pub use continuity::{ContinuityReport, ContinuityState, RecoveryDirection, RecoveryOutcome};
pub use control::cachelito::Cachelito;
pub use entry::{CacheEntry, EntryState, Generation};
pub use entry::{CommitIntent, CommitToken, IntentKind};
pub use error::CacheError;
#[cfg(feature = "faults")]
pub use fault::{ArmedFault, DeterministicRng, FaultClass, FaultLedger, FaultPlan, OpKind};
pub use identity::{CacheContext, IdentityContext};
pub use integrity::{ContentDigest, IntegrityCheck, KeyFingerprint, Placement};
pub use key::{Key, KeyRef, MAX_KEY_SIZE, TENANT_SEPARATOR, frame_tenant_key};
pub use manager::CacheManager;
pub use policy::{
    CacheOperation, CachePolicy, CacheRequest, CacheState, DefaultPolicy, FailMode, PolicyDecision,
    PopulationStrategy, StrictPolicy,
};
pub use pool::MemoryPool;
pub use telemetry::{
    KeyIdentity, LatencyPercentiles, NoTelemetry, Operation, OperationRecord, Outcome,
    RingTelemetry, TelemetrySink,
};
pub use tier::fixed_tier_stub::FixedTierStub;
pub use tier::{BackendKind, CacheTier, TierHealth, TierId, TierRegistry};
pub use tier::{
    l0::L0Stub, l1::L1Stub, l2::L2Stub, l3::L3Stub, l4::L4Stub, l5::L5Stub, test::TestTier,
};

#[cfg(feature = "redis")]
pub use tier::backends::L3RedisBackend;
#[cfg(feature = "sled")]
pub use tier::backends::L4SledBackend;
#[cfg(feature = "oxigraph")]
pub use tier::backends::L5OxigraphBackend;
pub use tier::backends::{ByteValue, L5OriginBackend, OriginFetcher, OriginWriter};
