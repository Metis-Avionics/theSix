# CHANGELOG

## v1.0.0 — 2026-10-02

### Added
- `L6` authority tier — Postgres with pgvector is the intended binding. Excluded from the cache ladder: never blind-written, never invalidated, never selected as a fallback rung
- Capability reporting — `BackendKind`, `CacheTier::backend()` (required), `CacheManager::capabilities()`, `TierRegistry::has_tier()`. A consumer can now learn which backend a tier is bound to instead of discovering it by catching `TierUnavailable`
- `CacheError::CapacityExhausted`, replacing the `ConfigurationError` a full fixed-capacity tier returned. A full table is a runtime condition under load, not a misconfiguration
- `L5OxigraphBackend` (`feature = "oxigraph"`) — RDF/SPO backend; each entry is one quad, so entries are queryable with SPARQL. Built with `default-features = false`, because oxigraph's default feature is `rocksdb`
- `tests/await_safety.rs` — proves no control-plane shard guard is held across an `.await`, plus a watchdog test proving the harness would notice a block

### Changed
- **`CacheTier` is now `async` (breaking)** — via `#[async_trait]`, because the trait is held as `Arc<dyn CacheTier<V>>` and native AFIT is not dyn-compatible. `CacheManager`'s methods were already async; this was the last synchronous edge in the data path. The control plane stays synchronous
- **L3/L4/L5 no longer refuse every operation (breaking)** — in 0.2.x they returned `Err(TierUnavailable` unconditionally, leaving a default six-rung manager permanently failing on its upper three rungs. They now store values via a sharded `FixedTierStub` and self-report as `InMemoryFallback`, so an upper rung cannot read as the distributed or durable store it nominally is
- In-memory tier stubs are sharded — one `Mutex` per tier serialised all access to L0. Copies `Cachelito`'s pre-allocated shard pattern; `DashMap` remains rejected (allocates after init, and its caller-chosen guard lifetime deadlocks a shard when held across an await)
- `specs/tiers.toml` records `count = 7`, `[tier.l6]`, and `routing.last_cache_tier`

### Fixed
- `cargo deny` advisories: RUSTSEC-2026-0194 and RUSTSEC-2026-0195 on `quick-xml 0.37.5`, reached only as `oxrdfxml -> oxrdfio -> oxigraph` under `--all-features`. Not fixable in-tree (`oxrdfxml` pins `quick-xml = "0.37"`, patched release is `>= 0.41.0`) and unreachable from any thesix feature, so both are ignored with a documented reason in `deny.toml`
- Shard count rounds up to a power of two. Rounding down stranded shards the mask could never produce: with 3 shards and mask 2 the index was only ever 0 or 2
- `tier_for` silently substituted L0 for an unbound tier; `has_tier` now distinguishes substituted from bound

### Not included
- Postgres, Neo4j and HelixDB backends. Each needs a live service to verify, and an unrunnable tier in a cache library is a liability rather than a feature. They follow once there is a service to test against.


## v0.2.3 — 2026-09-14

### Added
- `examples/polars_etl.rs` — ETL over Polars: versioned marts, single-flight load under a 20-reader stampede, invalidate on new batches (dev-dependency on `polars`)
- `examples/polars_elt.rs` — ELT over Polars: raw lake with transform-on-read and stale-while-revalidate `refresh` (serves the previous mart when a refresh fails)

## v0.2.2 — 2026-09-14

### Fixed
- README install snippets point at `0.2.1` (were the `0.2` semver range)

## v0.2.1 — 2026-09-14

### Added
- `examples/quickstart.rs` — runnable quick start mirroring the README

### Changed
- README rewritten as a crate README (was the original design/planning doc): accurate install, quick start, architecture, policy, single-flight, API reference, and quality-gate sections; stale signatures, `dashmap`/`moka` references, and "do not publish" removed

## v0.2.0 — 2026-09-14

### Added
- `CacheContext` builder carrying `IdentityContext` + optional TTL; threaded through all operations
- `exists()` and `refresh()` (stale-while-revalidate) operations
- `StrictPolicy` — authz-deny for anonymous mutating operations
- TTL/expiration tracked in the control plane and tiers; lazy `Ready -> Stale` transition
- Populate retry: max 3 attempts, exponential backoff, fail-open/closed honoured
- `waiters_receive_population_error` via a control-plane error marker
- `ByteValue` codec + real feature-gated backends: `L3RedisBackend`, `L4SledBackend` (persistent), `L5OriginBackend`
- Policy precedence ladder (tier-health, capacity) and an `availability` signal on `TierHealth`
- `deny.toml`; CI gates for `cargo deny`, `cargo machete`, `miri`, and `cargo publish --dry-run`

### Changed
- **BREAKING**: all `CacheManager` operations now take a `&CacheContext` (identity + TTL via builder)
- `CacheError` is now `Copy`
- `TierRegistry` is interior-mutable with real `fail`/`recover` wired to tier outcomes
- `Cachelito` pre-allocates its control pool; DashMap removed (fixed-capacity sharded slot map)
- Edition 2021 -> 2024; MSRV 1.88 -> 1.98 (`gen` renamed — reserved keyword in edition 2024)

### Fixed
- `Cachelito::acquire` CAS now claims from the actual entry state (Failed/Stale reclaimable for retry)
- Populate tier now matches the policy decision waiters observe (fixes a waiter `Miss` regression)
- Stale `lib.rs` doctest corrected; doc example compiles

### Security
- Real authn gate: unauthenticated requests rejected with `CacheError::Unauthenticated` pre-policy

### Removed
- DashMap dependency; dead `moka` feature

## v0.1.0 — 2026-09-13

### Added
- Initial implementation of theSix policy-driven six-tier cache orchestration library
- `CacheManager<K, V, P>` — public API with get, get_or_fetch, set, invalidate, remove, promote, demote
- `Cachelito` — control-plane state registry using sharded DashMap
- `CachePolicy<K, V>` trait — policy engine with `DefaultPolicy` implementation
- `CacheTier<V>` trait — tier abstraction with L0–L5 stub implementations
- `CacheError` — structured error model (Unauthenticated, Unauthorized, Miss, TierUnavailable, StaleGeneration, Timeout, etc.)
- `IdentityContext` — authentication context with anonymous support
- `Generation` — generation-based invalidation protection
- Single-flight population coordination with waiter timeout and owner cancellation
- Tier failure isolation with circuit breaker (fail-open/fail-mode)
- Authn gate at CacheManager entry point
- Authz check via policy engine (authorized flag in PolicyDecision)
- 18 tests across 5 test files covering all 20 README scenarios
- 8 benchmarks for cache operations
- 6 TOML specification files under `specs/`
- `living.toml` — living documentation with handover, session, and changelog tracking

### Fixed
- test_invalidation assertion corrected (get after set returns value, not Miss)
- test_complete_six_tier_integration assertion corrected (get after remove returns Miss)
- mark_population_start uses entry API to handle new keys
- become_population_owner wraps fetch in timeout
- All clippy warnings resolved across source and test files
- All formatting issues resolved

### Quality Gates
- `cargo fmt --check` — pass
- `cargo check --all-targets --all-features` — pass
- `cargo test` — 18/18 pass
- `cargo clippy --all-targets --all-features -- -D warnings` — pass
- `cargo doc --no-deps` — pass
- `cargo package --list` — pass
- `cargo publish --dry-run` — pass
