# AGENTS.md — theSix

## Project

Rust library for policy-driven six-tier cache orchestration. Crate name: `thesix`.

## Build & Test

```bash
cargo build
cargo test                    # all tests (29 integration + doctests)
cargo test --all-targets --all-features
cargo test --test integration  # single test file
cargo test test_single_flight  # single test by name pattern
```

Benchmarks (requires `--features=bench` or dev-deps already included):
```bash
cargo bench
```

## Quality Gates (must all pass before considering complete)

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo doc --no-deps
cargo package --list
cargo publish --dry-run
cargo deny check              # needs deny.toml (present)
cargo machete                 # unused-dependency scan
```

**Order matters**: fmt → check → clippy → test → doc → package → publish-dry-run.

## Architecture (critical)

**Control plane vs data plane** — this is the core design constraint:
- `Cachelito` (control plane): tracks state only — entry state, generation, tier, population ownership, tier health. **Never stores application payloads.**
- `CacheTier` implementations (data plane): store/retrieve actual values.
- `CacheManager` sits between: enforces auth, delegates tier selection to policy, coordinates via Cachelito.

**No control guard may cross `.await` or I/O.** Cachelito's `acquire()` returns a `ControlSnapshot` with `Arc<Notify>` — wait on the notify, not on a shard guard. The control plane is a pre-allocated sharded slot map (no DashMap; TETANUS Rule 3).

## Module Layout

```
src/
├── lib.rs              # crate root, re-exports, documentation
├── manager.rs          # CacheManager (public API)
├── control/mod.rs      # → cachelito.rs (NOT src/control.rs)
├── control/cachelito.rs # Cachelito control plane
├── policy.rs           # CachePolicy trait, Default+Strict policies, CacheOperation, precedence ladder
├── tier/mod.rs         # TierId, TierRegistry (NOT src/tier.rs)
├── tier/trait.rs       # CacheTier trait, TierHealth (aliased as tier_trait)
├── tier/l0.rs – l5.rs  # Tier stubs (fixed-capacity, Fallible with_capacity)
├── tier/test.rs        # TestTier (health-controllable for failure tests)
├── tier/fixed_tier_stub.rs # FixedTierStub (slot map + TTL) backing L0-L2/TestTier
├── tier/backends/      # ByteValue codec + L3RedisBackend / L4SledBackend / L5OriginBackend
├── entry.rs            # Generation, EntryState, CacheEntry
├── error.rs            # CacheError enum (Copy)
├── identity.rs         # IdentityContext + CacheContext builder
├── key.rs              # Key trait + KeyRef borrowed key view
└── pool.rs             # MemoryPool fixed-capacity allocator
```

**Note**: `src/control.rs` and `src/tier.rs` do NOT exist — modules are in `mod.rs` and subdirectories. `src/metrics.rs` also does not exist (listed in README but not implemented).

## Key Types

- `CacheManager<K, V, P>` — generic over key type `K`, value type `V`, and policy type `P`. All three must be specified.
- `Cachelito` — no generic parameters. Use `Cachelito::new()` or `with_shards(n)`.
- `TierRegistry` — tracks all 6 tiers. `TierId::L0` through `TierId::L5`.
- `ControlSnapshot` — returned by `Cachelito::acquire()`. Contains state copy + `Arc<Notify>` for waiting. **Do not hold across await.**

## Auth Model

- `CacheManager::check_auth()` enforces `IdentityContext::is_authenticated()`; unauthenticated -> `CacheError::Unauthenticated` (pre-policy).
- Authorization is policy-driven: `PolicyDecision.authorized` gate applied to ALL mutating ops (set/invalidate/remove/promote/demote/refresh) + reads.
- Identity is supplied per-request via `CacheContext`; `DefaultPolicy` is permissive, `StrictPolicy` denies anonymous writes.
- `CacheError::Unauthenticated` and `CacheError::Unauthorized` are distinct error variants.

## Public API Surface

`CacheManager` methods, all taking `&CacheContext` (identity + TTL builder):
`get`, `get_or_fetch`, `set`, `invalidate`, `remove`, `exists`, `refresh`, `promote`, `demote`.

**Breaking change (v0.2.0)**: every operation signature changed from `(&self, key, ...)` to `(&self, key, ctx: &CacheContext, ...)`. Build the context once per request:
`CacheContext::new(IdentityContext::new(principal, roles, tenant))` or `CacheContext::anonymous()`.

Policies: `DefaultPolicy` (permissive baseline) and `StrictPolicy` (denies anonymous writes). Implement `CachePolicy` for custom authz.

**Admin/test-only** (not part of the data-plane API):
- `CacheManager::tier(&TierId)` — returns `Arc<dyn CacheTier<V>>` for test/admin access
- `CacheManager::cachelito()` — returns `&Cachelito` for test access
- `CacheManager::with_timeout(policy, cachelito, registry, tiers, pool, duration)` — custom wait timeout

## Real Backends (feature-gated)

In addition to in-memory stubs, real backends exist behind features and require `V: ByteValue`:
- `L3RedisBackend` (`feature = "redis"`) — distributed, sync connection
- `L4SledBackend` (`feature = "sled"`) — persistent, embedded sled; stores TTL prefix + lazy eviction
- `L5OriginBackend` — origin-fallback via pluggable `OriginFetcher`/`OriginWriter` callbacks

`moka` was removed as a dead feature; reintroduce when implemented. The core `CacheTier<V>` trait is unchanged (sync, `Clone + Send + Sync + 'static`).

## Tier Stubs

Tier stubs (L0-L5, TestTier) wrap `FixedTierStub` (fixed-capacity slot map). `new()` is infallible (panics only on startup pool-alloc failure — sanctioned init mode); `with_capacity(n)` is fallible and returns `Result` (rejects capacity 0).

## Test Patterns

Integration tests share a `make_manager()` helper pattern. Stampede tests use `make_manager_with_timeout()` with short durations (100ms–2s) to trigger timeout paths.

`TestTier` (in `src/tier/test.rs`) has `set_healthy(bool)` for simulating tier failure in `test_tier_failure` and `test_tier_recovery`.

## Crate Features

Optional backend features `redis` and `sled` are implemented (see Real Backends). `moka` was removed as a dead feature. The default feature build is in-memory stubs only.

## CI

`.github/workflows/ci.yml` runs on push/PR to `main` (Rust 1.98). Jobs: fmt, check, test (incl. doctest), clippy `-D warnings`, doc, package + publish-dry-run, plus TETANUS static-analysis gates: cargo-deny, cargo-machete, miri (no-op; `#![forbid(unsafe_code)]`).

## Important Constraints

1. Application code MUST NOT select cache tiers directly
2. Cachelito MUST NOT store payload data
3. No control guard across `.await` or I/O
4. No global lock around cache hierarchy
5. Single-flight: only one population per key at a time
6. Generation-based invalidation: stale results rejected
7. Tier failure isolation via circuit breaker (5 consecutive failures = circuit open); registry health updated from real tier outcomes
8. `CacheError` is the single error type for all failures (structured, not strings)

## Repository Config

- Edition: **2024**
- Rust toolchain / MSRV: **1.98**
- No `kilo.json` at repo root (Kilo config is in `.kilo/` directory)
- `living.toml` tracks handover/session/changelog state
