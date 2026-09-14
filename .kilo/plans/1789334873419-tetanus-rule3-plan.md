# Plan: TETANUS Full Compliance for theSix

## Goal

Bring PR1 and the `thesix` crate into full compliance with all 10 TETANUS rules, treating the crate as safety-critical. This requires correctness/security fixes plus breaking API changes to satisfy Rule 3 (no dynamic allocation after init).

## Scope

- `src/manager.rs`, `src/control/cachelito.rs`, `src/policy.rs`, `src/entry.rs`, `src/error.rs`, `src/identity.rs`, `src/lib.rs`
- `src/tier/**` (all tier stubs and trait)
- `tests/**`, `benches/**`
- `Cargo.toml`, `.github/workflows/ci.yml`, `AGENTS.md`, `CHANGELOG.md`
- `SPEC.tomL` (new or update existing specs)

## Out of Scope

- Optional backend features (`redis`, `moka`, `sled`) — remain unimplemented.
- Network/external I/O beyond the data plane.
- Changing the `V: Clone + Send + Sync + 'static` bound to `'static` only.

## Design Decisions

### D1: Rule 3 Boundary Definition

**Decision:** Rule 3 applies to the **control plane** and **tier storage allocation paths**. The data plane stores application payloads, which by definition requires memory. We satisfy Rule 3 by:

1. Pre-allocating all control-plane structures (`Cachelito` shards, `ControlEntry` pool) at `init`.
2. Introducing a caller-supplied `MemoryPool<V>` that owns a fixed-capacity slab allocator. Tiers acquire/release slots from this pool; no heap allocation occurs in `set`, `get`, `invalidate`, `remove`, `promote`, `demote`, or `get_or_fetch` after init.
3. Keys are pre-encoded into fixed-size byte arrays (`[u8; KEY_SIZE]`) or callers supply `KeyRef<'_>` borrowed from a pre-allocated key buffer. No `format!("{:?}", key).into_bytes()` in hot paths.
4. `Vec<u8>` is allowed only in initialization and key-pool management code, not in the data/control path after init.

**Rationale:** This is the closest a general-purpose cache library can get to Rule 3 without breaking the public API entirely. It makes allocation explicit, bounded, and auditable.

### D2: Breaking API Shape

**Decision:** Introduce three breaking changes:

1. **`KeyRef<'a>`** replaces ad-hoc `Vec<u8>` in public APIs:
   ```rust
   pub struct KeyRef<'a>(&'a [u8]);
   ```
   Callers encode keys externally or use a provided key pool. `KeyRef` is `Copy` and carries no allocation.

2. **`MemoryPool<V>`** is passed at `CacheManager` construction:
   ```rust
   pub struct MemoryPool<V> { /* fixed-capacity slab */ }
   pub struct CacheManager<K, V, P, const N: usize> {
       pool: MemoryPool<V>,
       // ...
   }
   ```
   `N` is the maximum entry count. Tier stubs use the pool instead of `DashMap`/`Vec::new()`.

3. **`CacheManager::new` signature changes:**
   ```rust
   impl<K, V, P> CacheManager<K, V, P, const N: usize>
   where
       K: Key + Send + Sync,
       V: Clone + Send + Sync + 'static,
       P: CachePolicy<K, V>,
   {
       pub fn new(
           policy: P,
           cachelito: Cachelito,
           registry: TierRegistry,
           tiers: Vec<Arc<dyn CacheTier<V>>>,
           pool: MemoryPool<V>,
       ) -> Self { ... }
   }
   ```

**Rationale:** This makes allocation explicit at construction, eliminates runtime allocation in hot paths, and enables static analysis of memory bounds.

### D3: Key Encoding

**Decision:** Remove `key_to_bytes` from `CacheManager`. Require `K: Key`:
```rust
pub trait Key: Hash + Eq + Clone + Send + Sync + Debug {
    fn encode(&self, buf: &mut [u8]) -> Result<usize, CacheError>;
    fn encoded_len(&self) -> usize;
}
```
Callers provide a key pool or encode into a stack buffer. `KeyRef` wraps the borrowed bytes.

**Rationale:** Eliminates per-call `format!("{:?}", key).into_bytes()` allocation. Encoding is explicit and bounded.

### D4: Tier Storage Redesign

**Decision:** Replace `DashMap<Vec<u8>, V>` in tier stubs with a fixed-capacity slot map backed by `MemoryPool<V>`. Tier stubs no longer allocate after init.

- L0/L1/L2: use `[Option<SlotRef<V>>; N]` with open-addressing or linear probing.
- L3/L4/L5: same structure; `TierUnavailable` remains for unimplemented backends.

`SlotRef<V>` is an index/offset into the pool, not an owned value.

**Rationale:** Satisfies Rule 3 for data-plane storage. Tier stubs become stateless dispatchers over pre-allocated slots.

### D5: Control Plane Redesign

**Decision:** `Cachelito` pre-allocates all `ControlEntry` instances at `with_shards`. `acquire` must claim ownership atomically:

```rust
pub fn acquire(&self, key: &KeyRef<'_>, tier: TierId) -> Result<ControlSnapshot, CacheError> {
    // CAS or entry API to set state to InFlight + population_owner = true
    // If already InFlight, return snapshot with notify
}
```

`mark_population_start` is removed. Ownership claim happens in `acquire`.

**Rationale:** Fixes the single-flight race. Eliminates post-init allocation in the control plane.

### D6: Correctness Fixes Integrated with TETANUS

**Decision:** Fix all review findings as part of the refactor, not as afterthoughts:

1. **Authn/Authz:** `check_auth` accepts `&IdentityContext`. If `!identity.is_authenticated()` return `Unauthenticated`. Policy check applied to all public operations.
2. **Single-flight:** Atomic ownership claim in `acquire` (D5).
3. **Generation safety:** `invalidate` increments generation atomically; `publish` checks captured generation + ownership before accepting.
4. **Policy state:** Pass real `CacheState` (from `acquire` snapshot) to `select`, not `CacheState::new()`.
5. **Tier health:** Move health to `TierRegistry`/`TierId` level; `fail` updates tier health, not per-key.
6. **Atomic promote/demote:** Use a compare-and-swap on control tier or a two-phase commit with rollback.

### D7: Assertions and Parameter Validation (Rule 5, Rule 7)

**Decision:** Add `assert!` and explicit validation at every public entry point:

- `CacheManager` constructors validate tier count == 6 and pool non-zero.
- `acquire` validates key length > 0.
- `publish` asserts generation matches captured generation.
- `tier_for` asserts `tier_id.as_usize() < tiers.len()`.
- No `.unwrap()` or `.expect()` in public or safety-critical paths. Use `?` or `if !cond { return Err(...) }`.

### D8: Loop Bounds (Rule 2)

**Decision:** All loops in tiers and control plane use explicit counters with compile-time or init-time maxima:

- Tier slot lookup: bounded by `N` (const generic).
- `try_fallback_tier`: bounded by `registry.len()`.
- No unbounded `loop` or `while` without counters.

### D9: Function Length (Rule 4)

**Decision:** Refactor `become_population_owner`, `try_fallback_tier`, and `acquire` into sub-functions if they exceed ~60 lines. Target: no public function > 60 lines.

### D10: Pointer and Macro Discipline (Rule 8, Rule 9)

**Decision:**
- No `macro_rules!` beyond simple constructors. Remove or inline any complex macros.
- No `Box<Box<T>>`, `Arc<Arc<T>>`, `&(&T)`. `Arc<dyn CacheTier<V>>` is one level of trait-object indirection; acceptable.
- No `cfg` gating in safety-critical paths beyond `#[cfg(test)]`.

### D11: CI and Packaging (Rule 10)

**Decision:**
- Add `#![deny(warnings)]` and `#![warn(clippy::pedantic)]` to `src/lib.rs`.
- Add `cargo deny check`, `cargo machete`, and `cargo +nightly miri test` to CI.
- Exclude `.kilo/plans/`, `living.toml`, `AGENTS.md`, `CHANGELOG.md`, `specs/` from `cargo package` via `Cargo.toml` `exclude`.
- Quote `"on":` in CI YAML.
- Add `cargo publish --dry-run` to CI.

### D12: Documentation and Spec Updates

**Decision:**
- Update `AGENTS.md`, `CHANGELOG.md`, and `SPEC.toml` to reflect breaking API changes.
- Update `lib.rs` doc example to use `KeyRef` and `MemoryPool`.
- Remove or mark unimplemented claims (`refresh`, `exists`, per-tier circuit breaker, TTL).

## Task List

### Phase 1: Foundation and API Redesign

1. Add `Key` trait and `KeyRef<'a>` type in `src/entry.rs` or new `src/key.rs`.
2. Implement `Key` for `String`, `&str`, `Vec<u8>` in `src/identity.rs` or key module.
3. Add `MemoryPool<V>` in `src/pool.rs` with fixed-capacity slab allocator.
4. Update `CacheManager` struct to include `pool: MemoryPool<V>` and const generic `N`.
5. Update `CacheManager::new`/`with_timeout` signatures.
6. Remove `key_to_bytes` from `CacheManager`.

### Phase 2: Control Plane Refactor

7. Redesign `Cachelito::with_shards` to pre-allocate `ControlEntry` pool.
8. Implement atomic ownership claim in `acquire` using entry API.
9. Remove `mark_population_start`; merge its logic into `acquire`.
10. Fix `publish` to validate captured generation + ownership.
11. Fix `release`/`invalidate` to increment generation.
12. Move `tier_health` from per-key to per-tier in `TierRegistry`.

### Phase 3: Data Plane Refactor

13. Replace `DashMap` in L0/L1/L2 stubs with fixed-capacity slot maps backed by `MemoryPool<V>`.
14. Update `CacheTier` trait if needed to accept `KeyRef` and pool references.
15. Ensure no `Vec::new()`, `Box::new()`, `DashMap::new()` in tier hot paths after init.

### Phase 4: Correctness and Security Fixes

16. Implement authn gate in `check_auth` using `IdentityContext`.
17. Apply policy authorization to all public operations.
18. Fix atomic promote/demote with compare-and-swap on control tier.
19. Pass real `CacheState` from `acquire` snapshot to policy `select`.
20. Add parameter validation and assertions per Rule 5/7.

### Phase 5: TETANUS Structural Compliance

21. Refactor functions > 60 lines.
22. Add explicit loop counters with bounds.
23. Remove/replace complex macros.
24. Audit pointer levels; remove double indirection.
25. Add `#![deny(warnings)]` and clippy pedantic to `lib.rs`.
26. Update tests and benchmarks to use new API.

### Phase 6: CI, Packaging, and Docs

27. Update `.github/workflows/ci.yml` with TETANUS validation gates.
28. Update `Cargo.toml` `exclude` to remove non-package artifacts.
29. Update `AGENTS.md`, `CHANGELOG.md`, `SPEC.toml` to reflect changes.
30. Update `lib.rs` doc example and module docs.
31. Update/remove stale claims about unimplemented features.

## Validation

- `cargo fmt --check`
- `cargo check --all-targets --all-features`
- `cargo test --all-targets --all-features` (benchmarks run as part of `--all-targets`)
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo doc --no-deps`
- `cargo package --list` (verify excluded artifacts)
- `cargo publish --dry-run`
- `cargo deny check`
- `cargo machete --workspace`
- `cargo +nightly miri test` (if any `unsafe` introduced for pool/slots)

## Risks

1. **Breaking API adoption:** Downstream crates (`theDAF`) must update. Coordinate release as semver major.
2. **Performance:** Fixed-capacity pools and linear probing may degrade vs `DashMap`. Benchmark and tune.
3. **Complexity:** `MemoryPool` and slot maps add implementation surface. Keep them internal; expose only config at init.
4. **Rule 3 strictness:** If the project later wants to drop strict Rule 3, the pool abstraction allows switching to runtime allocation without changing public APIs.

## Open Questions

None. Proceed to implementation.
