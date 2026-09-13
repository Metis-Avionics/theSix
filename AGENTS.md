# AGENTS.md — theSix

## Project

Rust library for policy-driven six-tier cache orchestration. Crate name: `thesix`.

## Build & Test

```bash
cargo build
cargo test                    # all tests (18 unit + integration + doctests)
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
```

**Order matters**: fmt → check → clippy → test → doc → package → publish-dry-run.

## Architecture (critical)

**Control plane vs data plane** — this is the core design constraint:
- `Cachelito` (control plane): tracks state only — entry state, generation, tier, population ownership, tier health. **Never stores application payloads.**
- `CacheTier` implementations (data plane): store/retrieve actual values.
- `CacheManager` sits between: enforces auth, delegates tier selection to policy, coordinates via Cachelito.

**No control guard may cross `.await` or I/O.** Cachelito's `acquire()` returns a `ControlSnapshot` with `Arc<Notify>` — wait on the notify, not on a DashMap guard.

## Module Layout

```
src/
├── lib.rs              # crate root, re-exports, documentation
├── manager.rs          # CacheManager (public API)
├── control/mod.rs      # → cachelito.rs (NOT src/control.rs)
├── control/cachelito.rs # Cachelito control plane
├── policy.rs           # CachePolicy trait, DefaultPolicy, CacheOperation, etc.
├── tier/mod.rs         # TierId, TierRegistry (NOT src/tier.rs)
├── tier/trait.rs       # CacheTier trait, TierHealth (aliased as tier_trait)
├── tier/l0.rs – l5.rs  # Tier stubs
├── tier/test.rs        # TestTier (health-controllable for failure tests)
├── entry.rs            # Generation, EntryState, CacheEntry
├── error.rs            # CacheError enum
└── identity.rs         # IdentityContext (authn)
```

**Note**: `src/control.rs` and `src/tier.rs` do NOT exist — modules are in `mod.rs` and subdirectories. `src/metrics.rs` also does not exist (listed in README but not implemented).

## Key Types

- `CacheManager<K, V, P>` — generic over key type `K`, value type `V`, and policy type `P`. All three must be specified.
- `Cachelito` — no generic parameters. Use `Cachelito::new()` or `with_shards(n)`.
- `TierRegistry` — tracks all 6 tiers. `TierId::L0` through `TierId::L5`.
- `ControlSnapshot` — returned by `Cachelito::acquire()`. Contains state copy + `Arc<Notify>` for waiting. **Do not hold across await.**

## Auth Model

- `CacheManager::check_auth()` is currently a no-op (stub). Real auth is application-level.
- Authorization is policy-driven: `PolicyDecision.authorized` gate in `CacheManager::get()`/`set()`.
- `IdentityContext::anonymous()` is used internally; applications can supply their own via future API extensions.
- `CacheError::Unauthenticated` and `CacheError::Unauthorized` are distinct error variants.

## Public API Surface

`CacheManager` methods: `get`, `get_or_fetch`, `set`, `invalidate`, `remove`, `promote`, `demote`.

**Admin/test-only** (not part of the data-plane API):
- `CacheManager::tier(&TierId)` — returns `Arc<dyn CacheTier<V>>` for test/admin access
- `CacheManager::cachelito()` — returns `&Cachelito` for test access
- `CacheManager::with_timeout(policy, cachelito, registry, tiers, duration)` — constructor with custom wait timeout

## Tier Stubs

L0/L1/L2 use `#[derive(Debug, Default)]`. **L3/L4/L5 require manual `Default` impls** because `PhantomData<V>` cannot derive `Default` without `V: Default` bound. Use `L3Stub::new()` / `L3Stub::<T>::default()`.

## Test Patterns

Integration tests share a `make_manager()` helper pattern. Stampede tests use `make_manager_with_timeout()` with short durations (100ms–2s) to trigger timeout paths.

`TestTier` (in `src/tier/test.rs`) has `set_healthy(bool)` for simulating tier failure in `test_tier_failure` and `test_tier_recovery`.

## Crate Features

Optional backend features exist in `Cargo.toml` but are not implemented: `redis`, `moka`, `sled`. All tiers currently use in-memory stubs.

## CI

`.github/workflows/ci.yml` runs on push/PR to `main` with Rust 1.75. Jobs: check, test, clippy, doc, package.

## Important Constraints

1. Application code MUST NOT select cache tiers directly
2. Cachelito MUST NOT store payload data
3. No control guard across `.await` or I/O
4. No global lock around cache hierarchy
5. Single-flight: only one population per key at a time
6. Generation-based invalidation: stale results rejected
7. Tier failure isolation via circuit breaker (5 consecutive failures = circuit open)
8. `CacheError` is the single error type for all failures (structured, not strings)

## Repository Config

- Edition: **2021** (README mentions 2024 but Cargo rejects it due to `rust-version = "1.75"` mismatch)
- Rust toolchain: 1.75
- No `kilo.json` at repo root (Kilo config is in `.kilo/` directory)
- `living.toml` tracks handover/session/changelog state
