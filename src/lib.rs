//! # theSix
//!
//! Policy-driven six-tier cache orchestration for Rust.
//!
//! ## Architecture
//!
//! theSix separates the cache system into a **control plane** and a **data plane**:
//!
//! ```text
//! Application
//!   │
//!   ▼
//! CacheManager      ← API / orchestration
//!   │
//!   ▼
//! Policy Engine     ← decides what should happen
//!   │
//!   ▼
//! Cachelito         ← coordinates concurrent state (control plane)
//!   │
//!   ▼
//! Six-Tier Data Plane ← actual cache implementations
//!   ├── L0 request-local
//!   ├── L1 hot-local
//!   ├── L2 local
//!   ├── L3 distributed
//!   ├── L4 persistent
//!   └── L5 origin-fallback
//! ```
//!
//! The application should never need to know that L3 happens to be Redis.
//!
//! ## CacheManager
//!
//! `CacheManager` is the public API. It exposes cache operations
//! (`get`, `get_or_fetch`, `set`, `invalidate`, `remove`, `promote`, `demote`)
//! and enforces authentication and authorization before any state is touched.
//!
//! Tier selection is delegated to the policy engine; the application never
//! selects a tier directly.
//!
//! ## Cachelito
//!
//! Cachelito is the control-plane state registry. It uses a sharded
//! `DashMap` to track per-key control state (entry state, generation, tier,
//! population ownership, tier health) without storing application payloads.
//!
//! Key invariants:
//! - Control state does not store payload data.
//! - Control guards must not cross `.await` points.
//! - Control guards must not cross I/O.
//!
//! ## Policy Engine
//!
//! The policy engine determines tier selection based on operation type,
//! key metadata, entry metadata, cache state, tier health, and identity.
//!
//! The `CachePolicy` trait is the abstraction; `DefaultPolicy` is a basic
//! implementation. Custom policies can be supplied via the `P` type parameter
//! on `CacheManager`.
//!
//! ## Six Logical Tiers
//!
//! | Tier | Role | Scope | Persistent |
//! |------|------|-------|------------|
//! | L0 | Request-local | Request | No |
//! | L1 | Hot-local | Process | No |
//! | L2 | Local | Process | No |
//! | L3 | Distributed | Cluster | No |
//! | L4 | Persistent | Host | Yes |
//! | L5 | Origin-fallback | External | No |
//!
//! Backend implementations are configurable and not part of the core contract.
//!
//! ## Single-Flight Semantics
//!
//! For a given key, only one caller may populate the cache at a time.
//! Other callers wait for the population to complete. This prevents
//! cache stampedes on concurrent misses.
//!
//! Flow:
//! - First caller becomes population owner → fetches → publishes
//! - Other callers wait → join existing population
//! - Owner failure releases state; waiters receive the error
//!
//! ## Generation Invalidation
//!
//! Every population captures the current generation. If a newer generation
//! exists (due to invalidation), stale population results are rejected.
//!
//! ```text
//! generation 41 → population starts
//! generation 42 → invalidation
//! population 41 completes → rejected (stale)
//! ```
//!
//! ## Concurrency Guarantees
//!
//! - Multiple concurrent readers are supported.
//! - Concurrent reads and writes on unrelated keys do not block each other.
//! - Contention on the same key is coordinated via Cachelito's single-flight.
//! - No global lock around the entire cache hierarchy.
//! - No DashMap guard held across await points.
//!
//! ## Failure Modes
//!
//! - **Tier unavailable**: Policy may route to next tier (fail-open) or fail fast (fail-closed).
//! - **Population failure**: Owner failure releases in-flight state; waiters receive error.
//! - **Stale generation**: Population result rejected if generation changed.
//! - **Timeout**: Waiter or owner timeout releases state and returns `CacheError::Timeout`.
//! - **Unauthenticated**: Request rejected if no identity provided.
//! - **Unauthorized**: Request rejected if policy denies access.
//!
//! ## Example
//!
//! ```no_run
//! use thesix::{CacheManager, CacheTier, CachePolicy, IdentityContext};
//! use std::sync::Arc;
//!
//! // Create tier stubs, cachelito, policy, and registry
//! let cachelito = thesix::Cachelito::new();
//! let policy = thesix::DefaultPolicy;
//! let mut registry = thesix::TierRegistry::new();
//! let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
//!     Arc::new(thesix::L0Stub::<String>::new()),
//!     Arc::new(thesix::L1Stub::<String>::new()),
//!     Arc::new(thesix::L2Stub::<String>::new()),
//!     Arc::new(thesix::L3Stub::<String>::new()),
//!     Arc::new(thesix::L4Stub::<String>::new()),
//!     Arc::new(thesix::L5Stub::<String>::new()),
//! ];
//!
//! let manager: CacheManager<String, String, thesix::DefaultPolicy> =
//!     CacheManager::new(policy, cachelito, registry, tiers);
//!
//! // Application code never selects a tier:
//! // get_or_fetch takes a closure that returns a future:
//! // let value = manager.get_or_fetch(&"key", || async {
//! //     /* fetch from origin */
//! //     Ok("value".to_string())
//! // }).await;
//! ```
//!
//! ## Crate Layout
//!
//! | Module | Purpose |
//! |--------|---------|
//! | `src/manager` | `CacheManager` — public API and orchestration |
//! | `src/control` | `Cachelito` — control-plane state registry |
//! | `src/policy` | Policy engine, operations, decisions |
//! | `src/tier` | Tier abstraction and L0–L5 implementations |
//! | `src/entry` | Entry state, generation, cache entry |
//! | `src/error` | Structured error types |
//! | `src/identity` | Authentication context |

pub mod control;
pub mod entry;
pub mod error;
pub mod identity;
pub mod manager;
pub mod policy;
pub mod tier;

pub use control::cachelito::Cachelito;
pub use entry::{CacheEntry, EntryState, Generation};
pub use error::CacheError;
pub use identity::IdentityContext;
pub use manager::CacheManager;
pub use policy::{
    CacheOperation, CachePolicy, CacheRequest, CacheState, DefaultPolicy, FailMode, PolicyDecision,
    PopulationStrategy,
};
pub use tier::{
    l0::L0Stub, l1::L1Stub, l2::L2Stub, l3::L3Stub, l4::L4Stub, l5::L5Stub, test::TestTier,
};
pub use tier::{CacheTier, TierHealth, TierId, TierRegistry};
