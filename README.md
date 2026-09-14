# theSix

[![crates.io](https://img.shields.io/crates/v/thesix.svg)](https://crates.io/crates/thesix)
[![docs.rs](https://img.shields.io/docsrs/thesix)](https://docs.rs/thesix)
[![CI](https://github.com/Metis-Avionics/theSix/actions/workflows/ci.yml/badge.svg)](https://github.com/Metis-Avionics/theSix/actions)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Policy-driven six-tier cache orchestration for Rust. theSix is a cache
*orchestration* system, not merely a cache implementation: application code
never selects tiers, a policy engine routes every operation, and a control
plane (`Cachelito`) coordinates concurrent state — including single-flight
population so a cache miss never triggers a stampede.

```rust
let value = manager
    .get_or_fetch(&key, &ctx, || async { Ok("value".to_string()) })
    .await?;
```

No `manager.l3.get(...)`. That distinction is the whole point.

## Installation

```toml
[dependencies]
thesix = "0.2.1"
```

Optional real backends (in-memory stubs are the default):

| Feature | Backend | Notes |
|---------|---------|-------|
| `redis` | `L3RedisBackend` (distributed) | Synchronous client — call via `spawn_blocking` in async code |
| `sled`  | `L4SledBackend` (persistent) | Embedded sled; TTL prefix + lazy eviction |

```toml
thesix = { version = "0.2.1", features = ["redis", "sled"] }
```

Backends store bytes, so values must implement `ByteValue` (`Vec<u8>` and
`String` are provided; implement the two-method trait for your own types).

**Requirements:** Rust 1.98+, edition 2024, Tokio runtime (the public API is
`async`; the tier trait itself is synchronous by design).

## Quick start

```rust
use std::sync::Arc;
use thesix::{
    CacheContext, CacheManager, CacheTier, Cachelito, DefaultPolicy,
    IdentityContext, MemoryPool, TierRegistry,
    L0Stub, L1Stub, L2Stub, L3Stub, L4Stub, L5Stub,
};

#[tokio::main]
async fn main() -> Result<(), thesix::CacheError> {
    // Six tiers, dumb by design: policy + Cachelito decide everything.
    let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
        Arc::new(L0Stub::new()),
        Arc::new(L1Stub::new()),
        Arc::new(L2Stub::new()),
        Arc::new(L3Stub::new()),
        Arc::new(L4Stub::new()),
        Arc::new(L5Stub::new()),
    ];
    let manager: CacheManager<String, String, DefaultPolicy> = CacheManager::new(
        DefaultPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        tiers,
        MemoryPool::new(1024).expect("pool allocation failed"),
    );

    // One request context per call site: identity + optional TTL.
    let ctx = CacheContext::new(IdentityContext::new(
        "alice".to_string(),
        vec!["reader".to_string()],
        "tenant-1".to_string(),
    ))
    .with_ttl(std::time::Duration::from_secs(60));

    let key = "my-key".to_string();

    // Single-flight: 100 concurrent callers → exactly one fetch.
    let value = manager
        .get_or_fetch(&key, &ctx, || async { Ok("value".to_string()) })
        .await?;
    assert_eq!(value, "value");

    manager.set(&key, "new-value".to_string(), &ctx).await?;
    assert!(manager.exists(&key, &ctx).await?);
    manager.invalidate(&key, &ctx).await?;
    Ok(())
}
```

This exact program also lives in `examples/quickstart.rs` (`cargo run --example quickstart`).

Batch-analytics patterns live in `examples/polars_etl.rs` (ETL: versioned
marts, single-flight load under a 20-reader stampede, invalidate on new
batches) and `examples/polars_elt.rs` (ELT: raw lake with transform-on-read
and stale-while-revalidate `refresh`). Both need `--all-features` for the
Polars dev-dependency.

## How it works

```
Application
    │
    ▼
CacheManager  ── API / orchestration (authn gate, policy authz, coordination)
    │
    ▼
Policy Engine ── decides tier, operation, population strategy per request
    │
    ▼
Cachelito     ── control plane: entry state, generation, tier, TTL,
                   population ownership, tier health. Never stores payloads.
    │
    ▼
Six-Tier Data Plane ── stores/retrieves values only
    ├── L0 request-local
    ├── L1 hot-local
    ├── L2 local
    ├── L3 distributed (stub, or Redis with `redis` feature)
    ├── L4 persistent  (stub, or sled with `sled` feature)
    └── L5 origin/fallback (stub, or pluggable fetcher/writer)
```

**Control plane vs data plane** is the core constraint:

- `Cachelito` tracks state only. It must never store application payloads.
- `CacheTier` implementations store values only. They never decide routing.
- No control guard is ever held across `.await` or I/O — `acquire()` returns
  a `ControlSnapshot` with an `Arc<Notify>`; waiters sleep on the notify.
- The control plane is a pre-allocated sharded slot map (fixed capacity, no
  per-operation allocation after init).

## Identity and authorization

Every operation takes a `&CacheContext` (builder over `IdentityContext` plus
an optional TTL). Two gates apply:

1. **Authentication** (`CacheManager::check_auth`, pre-policy):
   unauthenticated callers get `CacheError::Unauthenticated`.
2. **Authorization** (policy-driven, on all mutating ops — set, invalidate,
   remove, promote, demote, refresh — plus reads): the policy's
   `PolicyDecision.authorized` flag gates the call, else
   `CacheError::Unauthorized`.

`DefaultPolicy` is a permissive baseline; `StrictPolicy` denies anonymous
writes. Implement `CachePolicy` for custom authz.

## Policy engine

Selection inputs per request: operation, key/entry metadata, live
`CacheState`, and per-tier health. Precedence ladder (first match wins):

1. explicit policy override → 2. tier health → 3. consistency →
   4. latency → 5. capacity → 6. default tier

Tier health carries an `availability` signal (0.0–1.0); five consecutive
failures open the circuit and the registry routes around the tier
(`TierRegistry::fail` / `recover` are fed by real tier outcomes).

## Single-flight and generations

A miss does not entitle every reader to populate:

```
MISS
               │
        ┌──────┴──────┐
        │             │
     ABSENT       IN_FLIGHT
        │             │
     become          wait
      owner            │
        │              │
     fetch             │
        │              │
     publish ◄─────────┘
```

- First caller becomes the population **owner**; others **wait** on the
  snapshot notify (bounded by a 5 s default timeout, configurable via
  `with_timeout`).
- Owner fetch is retried in place (max 3 attempts, exponential backoff);
  terminal errors propagate to waiters; fail-open policies fall back a tier.
- Every population captures a **generation**; `invalidate`/`remove` bump it
  and stale publications are rejected. TTL expiry lazily transitions
  `Ready → Stale` (served stale while revalidating by `refresh`).

## API reference

All ops are `async` and take `(&key, &ctx)` (`get_or_fetch`/`refresh` also
take a fetch closure returning `Result<V, CacheError>`):

| Method | Effect |
|--------|--------|
| `get` | Tier lookup per policy; lazy TTL expiry |
| `get_or_fetch` | `get`, else single-flight populate |
| `set` | Policy-routed write (authz-gated) |
| `invalidate` | Generation bump (stale writes rejected) |
| `remove` | Generation bump + tier eviction |
| `exists` | Presence check without fetching |
| `refresh` | Stale-while-revalidate |
| `promote` / `demote` | Policy-controlled tier movement |

Errors are the single `Copy` type `CacheError`: miss, tier unavailable,
policy/auth failures (`Unauthenticated` vs `Unauthorized`), population
failure, timeout, cancellation, stale generation, serialization,
configuration.

Admin/test-only surface (kept off the data path): `manager.tier(&TierId)`,
`manager.cachelito()`, `TestTier::set_healthy(bool)`.

## Testing and quality gates

29 integration tests prove the invariants — including 100-concurrent-requests
single-flight, owner cancellation, waiter timeout, tier failure/recovery, TTL
expiry, and strict-policy denial. Benchmarks in `benches/`.

```bash
cargo build
cargo test --all-targets --all-features
cargo bench
```

Every change must pass, in order: `cargo fmt --check` → `cargo check
--all-targets --all-features` → `cargo clippy --all-targets --all-features --
-D warnings` → `cargo test --all-targets --all-features` → `cargo doc
--no-deps` → `cargo package --list` → `cargo publish --dry-run` → `cargo deny
check` → `cargo machete`. CI enforces all of these plus miri (no-op guard;
the crate declares `#![forbid(unsafe_code)]`).

## Design notes

- theSix is synonymous with the **six-tier orchestration model**, not with
  any particular backend stack — L3 could be Redis today and something else
  tomorrow without touching application code.
- Non-goals (by design): distributed consensus, general persistence,
  application business logic, global cache coherence.
- Pre-1.0 the public API may still evolve (0.1 → 0.2 introduced
  `CacheContext`); pin exact versions and read `CHANGELOG.md`.

## License

MIT — see [LICENSE](LICENSE).
