# CHANGELOG

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
