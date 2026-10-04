# CHANGELOG

## Unreleased — verification architecture

The second review closed seven findings and could not confirm CI had executed the
head it approved. Chasing that found CI *could not* run for this branch at all, and
three more things underneath it that had never run either.

### Fixed

- **CI could not run for a stacked PR.** Both triggers were filtered to
  `branches: [main]`, and every PR here is stacked on a feature branch, so the
  filter excluded all of them. Every "green" gate result for this branch was a local
  fact.
- **Five mandatory gates executed in no CI job.** `xtask_unit` had none at all, so
  the tests for the code deciding which gates pass ran only on the maintainer's
  machine. `deny` and `machete` were shadowed by third-party actions that merely
  resemble the gates; `fmt` and `fuzz` bypassed theirs.
- **`check` and `clippy` never linted the gate runner or the test harness.** Both
  omitted `--workspace`, so only the root package was covered and four warnings
  accumulated in `xtask` unnoticed.
- **The gate runner leaked cargo's own package environment into every gate.**
  `CARGO_MANIFEST_DIR` and `CARGO_PKG_*` described *xtask's* package, so
  `cargo machete` walked the wrong tree — meaning `cargo xtask run machete` failed
  while the undocumented `./target/debug/xtask` invocation passed.
- **The fuzz job named an installer tag that does not exist**, so no fuzzer was
  installed and every smoke run failed looking like a crash. B7 fixed that job's
  shell syntax; the loop it fixed had never had a fuzzer to run.

### Added

- `[[verification.ci_job]]` in the contract plus `tests/contract/workflow.rs`, which
  parses the workflow and holds it to the contract in both directions: every gate
  covered, every cover real, every job declared, every reported check name matching.
- A `bash -n` lint over every `run:` block, with a regression test that feeds B7's
  exact stray `done` through the same path and requires it to be caught.
- `testkit::coverage::INVARIANT_PROOFS` and `[[verification.invariant_waiver]]`:
  all 95 declared invariants bound to a proving test (89) or waived with a stated
  reason (6), checked for exact set equality.
- A `Verification (all gates)` aggregator job, which is the single check branch
  protection requires. Matrix job names would silently stop matching.
- An `authorship` gate: no commit in this repository's history carries a
  co-authorship trailer. The rule is on the trailer *key*, and the banned keys live
  in `[verification.attribution]`, so the gate never has to contain an attribution in
  order to forbid one — a gate that grepped for the literal string would have put it
  in its own source. It scans every ref rather than `HEAD`, so a rewritten commit that
  a sibling branch still reaches does not read as clean. Fifteen unit tests, including
  one that feeds the gate a co-authorship trailer and requires it to fail.

### Changed

- `main` is branch-protected: the aggregated gate is required, admins included,
  force-push and deletion disallowed.
- An abort of a write with an unknown outcome costs one repopulation — cheaper than
  serving a value nobody authorised.
- Co-authorship trailers were removed from seven commits on this branch, which
  rewrote their ids and was force-pushed.

### The limit worth stating

The invariant registry proves a test is **named and exists**, not that it proves the
clause. Naming a test that ignores an invariant satisfies the gate. What it does
enforce is that an invariant cannot be declared into existence without something in
the repository being made responsible for it, and the six that have no honest proof
are listed with reasons rather than papered over.

## Unreleased — second review remediation

The three original findings were addressed, and a second review of that head found
the recovery path still carried B2's defect, plus a CI job that could not run.

### Fixed

- **An aborted write could become readable again.** `abort` restored the state the
  intent interrupted, which on an already-populated key meant `Ready` over a rung
  whose contents nobody had checked. A write that stored its bytes and then failed
  was served on the next read. The in-code justification — the read path consults
  the control plane first — held only while the entry stayed `Prepared`, and
  restoring `Ready` is what ended `Prepared`. `abort` now assumes residue and
  leaves the entry unservable; `abort_proven_clean` preserves the restoring
  behaviour for the two errors decided before any bytes are sent.
- **`CacheError` is asymmetric.** `WriteIndeterminate` says "my bytes may have
  landed" and no variant says the converse, so `TierUnavailable` and `Timeout`
  cannot be read as "they definitely did not". This is why the control plane must
  assume the worst rather than classify.
- **The blanket `IntegrityCheck` digest was one 64-bit hash run twice.** Two
  identically-seeded `DefaultHasher`s finish identically, so the 128-bit result was
  a deterministic transformation of one hash while the comment claimed two
  independent hashers. Replaced with two seeded FNV lanes.
- **The control plane identified entries by the number that chose their slot.**
  `integrity.rs` separates identity from placement precisely to avoid this, and the
  data plane followed; `Cachelito` did not. `KeyAddress` now holds a 128-bit
  fingerprint and a 64-bit placement hash, bundled so neither can be held without
  the other.
- **The CI fuzz job's shell block had an unmatched `done`.** GitHub runs a
  multiline `run:` as one script, so the step failed before the smoke loop started
  — and `continue-on-error: true` reported that as an allowed advisory failure.
  Every fuzz target the job claims to smoke-run had never actually run in CI.

### Changed

- An abort of a write whose outcome is unknown now costs one repopulation.
  Deliberate, and cheaper than serving a value nobody authorised.

### Tests

- `testkit::MisreportingTier`: stores bytes, then reports a failure that cannot be
  classified. Deliberately not a `FaultClass` — the contract already has an honest
  answer (`WriteIndeterminate`); this is the case it has no name for. A tier that
  fails cleanly stores no residue, so testing against one proves nothing, which is
  how the gap survived.
- `Cachelito::with_placement`, plus collision tests that fail against the conflated
  comparison: committing one key used to resolve the other, and two colliding keys
  produced one entry instead of two.
- Three tests rewritten because their premise was unsupported rather than because
  behaviour regressed — one of them required `abort` to restore `Ready`, i.e. it
  asserted the bug.

## v0.4.0 — architectural revision

The architecture is now stated as a machine-checked contract. [`theSix.toml`](./theSix.toml)
is the source of truth; `cargo xtask` executes the verification gates it declares and
refuses to claim success for a gate it could not run.

### Breaking: this release invalidates every existing cached key

`IdentityContext::tenant` is now part of the key. Previously the field existed and
**no line of the crate read it**, so two tenants using the same application key shared
one entry and one control-plane slot — a cross-tenant read that the authn/authz gates
could not prevent, because by the time they ran there was nothing left to separate.
Single-tenant deployments must expect one cold cache after upgrading.

### Breaking: rung substitution removed

`CacheManager::tier(id)` and `tier_for(id)` return `Option` and no longer hand back a
different rung. Previously `tier(&TierId::L6)` on a six-rung manager returned **L0**,
and a caller could not tell from the return value; on an empty rung list it panicked
outright. Use `bound_tier`, `nearest_bound_rung`, or `capabilities()`.

### Added

- **The contract as code.** `theSix.toml` declares the invariants, the verification
  gates, and the layer-to-target mapping. `tests/contract` asserts the crate agrees
  with it: version, rung count, fault taxonomy, negative cases, property invariants,
  and that every declared layer is bound to a real test target. `specs/*.toml` — six
  files that contradicted each other and the code — is deleted.
- **Two-phase commit with a control-plane intent.** `EntryState::Prepared`,
  `prepare`/`commit`/`abort`, and a payload-free intent record. An uncommitted write
  reads as a miss, so `partial_commit_visible = false` is now checkable rather than
  asserted. Recovery resolves by kind: a write aborts, a move completes forward.
- **Capability semantics as three orthogonal axes.** `CapabilityFlags` ×
  `OperationalState` × `DurabilityClass`, replacing a single mutually-exclusive enum
  that could not express "persistent *and* shared *and* degraded". All nine states the
  contract requires are representable, including `unbound` and `authoritative`.
- **Integrity.** `ContentDigest` over every stored value; a damaged record is reported
  as `CacheError::Corrupted` rather than served. `KeyFingerprint` (128-bit) makes
  placement collisions detectable, with an injectable `Placement` so a test can force
  one and demonstrate that two keys still do not alias.
- **Deterministic fault injection** behind a `faults` feature: eleven fault classes, a
  seedable plan, and a `FaultLedger` that records every activation so tests can prove
  their fault fired.
- **Tenant-framed keys** with an explicit `0x00` separator. An oversized frame is
  rejected, never truncated.
- **Structured observability.** `OperationRecord` with the eleven contract fields and
  an injectable `TelemetrySink`. No payload, and no key: a key is identified by a
  non-reversible digest plus a length.
- **Continuity states and recovery.** `ContinuityState`, `RecoveryDirection`,
  `RecoveryOutcome`, `RecoveryReport`, `CacheManager::continuity()` and `recover()`.
  A recovery report separates `recovered`, `failed` and `needs_reconciliation`,
  because "the sweep could not resolve this" and "the sweep lacks the information
  to" need different responses from a caller and were sharing a counter.
- **Move recovery is `external-reconciliation`, not `complete-forward`.** The
  contract previously promised a direction the runtime could not execute:
  finishing a move means reading the source rung and writing the destination, and
  the control plane persists only a key *hash*. Aborting is not a safe substitute
  either, since a move removes the source before it commits. The sweep now reports
  these intents and leaves them intact for an external reconciler.
- **New test layers**: `contract`, `negative`, `property`, `fault_injection`,
  `recovery`, `durability`, `security`, `performance`, `soak`, `loom`.
- **`cargo xtask`**, a clap-driven gate runner that reads the contract, plus a
  `justfile`.
- **A `merge_readiness` gate** driven by `bugs.toml`: it fails while any finding
  marked `blocks_merge` is open, and CI runs it as a blocking job. The expected
  finding counts are declared in `theSix.toml` rather than in the tracker, so a
  finding cannot be removed or downgraded to make the gate pass. A finding clears
  only by being `resolved` with a `rationale`, or `accepted-risk` with both
  `accepted_by` and `rationale`. nextest, sccache, and `clang` + `mold` when available.
- **Fuzzing.** Nine targets in an excluded `fuzz/` crate covering the contract's
  trust boundaries. Advisory in CI, since it needs nightly. The gate builds every
  target; a separate CI step smoke-runs each one and reports which target crashed
  rather than swallowing it. Running them found two wrong assertions *in the targets
  themselves* — a generation comparison that assumed `u64` wraps, and a framing check
  that asserted two differently-sized frames are both accepted. Neither was a crate
  defect, which is precisely why a target that cannot fail is worse than no target.

### Fixed — correctness

Each of these was a latent violation of a property the contract now states.

- **A cancelled `set` wedged the key's population path.** `set` ran
  prepare → await → commit with no RAII guard, so a future dropped at any await in
  between left the entry `Prepared` with `population_owner = true` and nobody to
  release it. `IntentGuard` now holds the intent across the write, in both `set`
  and the move path, and aborts it on drop. Fixing the move path also closed two
  leaks that had nothing to do with cancellation: the unbound-tier check and the
  key re-encode both returned while the entry was still `Prepared`.
- `StaleGeneration` returned from a population **without releasing ownership**. The
  entry stayed `InFlight` with an owner nobody would satisfy, so every later operation
  on that key waited out the full timeout and failed. A key could stay unusable until
  something invalidated it.
- `set` wrote `Generation::new(snapshot.generation + 1)` from a snapshot read *before*
  an `.await`, so an invalidation landing in that window was silently undone.
- `get` and `exists` called `acquire`, which **claims**. A read-only probe created a
  slot, took ownership, and left the key wedged. One failing read made a key
  permanently broken.
- `wait_and_get` read the rung and the owner's error from the waiter's own pre-wait
  snapshot, so waiters never received the owner's failure and read the wrong rung.
- A lost CAS in `acquire` reported the state the CAS *expected*, which looked
  claimable. The losing caller became a second population owner, and single-flight
  admitted two fetches for one key.
- `notify_one` stored one permit for a fan-out of 99 waiters, so 98 waited out the
  full timeout. Paired with `Notified::enable()` and a post-registration re-read,
  `notify_waiters` broadcasts without the lost-wakeup window.
- `refresh` never checked ownership, so every concurrent refresh became "the owner";
  it also swallowed every failure and returned `Ok(None)` when there was nothing stale
  to serve. On a committed entry it silently became a no-op.
- The fail-open fallback scanned `TierRegistry::all()`, which **includes the authority
  rung**, and replaced the original error with a bare `PopulationFailed`.
- `promote`/`demote` copied without removing the source and discarded the policy's tier
  choice, so they were not policy-controlled despite claiming to be.
- Open addressing did not survive deletion: `find_slot` stopped at the first empty
  slot, orphaning entries behind a hole. `get` returned `None` for values still
  stored, and the next write leaked a pool index. A soak over fill/reclaim cycles ran a
  table out of capacity at two-thirds full.
- `abort` overwrote a `commit` that had already won, making a committed value
  invisible. Found by a loom model of the exact interleaving.
- A prepared intent on a never-written key was invisible, because generation `0` was
  overloaded as the "no intent" sentinel. An invisible intent is an unrecoverable one.
- A failed write forced the entry to `Failed`, destroying the previously committed
  value. Aborting now restores the state the intent interrupted.
- `ShardedTierStub::capacity()` returned a hardcoded constant instead of the configured
  value.

### Fixed — availability and performance

- `set` hard-failed when its rung was full or down. It now walks down the ladder on any
  rung-level failure and retries a lost commit race, so a full rung is a routing
  decision rather than a caller-visible error.
- Data-plane reads were unbounded. A hung rung could pin a caller's task forever; every
  tier access now runs under the manager's wait bound, which is what makes
  "the control plane stays responsive while the data plane is stalled" true rather
  than partial.
- A failed population produced **no** telemetry record, so the failure rate and
  fallback rate the contract asks for were unmeasurable.
- Concurrent recovery sweeps reported more recoveries than there were intents, because
  aborting is idempotent and did not report what it actually did.

### Changed

- `Cargo.toml` uses `debug = 0` and `opt-level = 1` for dependencies. Debuginfo was
  15GB of a 98GB disk and the gate runner ran out of space mid-matrix, reporting
  nineteen failures that had nothing to do with the code.
- `BackendKind::is_native()` is replaced by `is_implemented()`. The old predicate
  answered `true` for `Moka`, `RocksDb`, `Postgres`, `Neo4j` and `Helix` — five backends
  the crate does not contain.
- The L3–L5 in-memory defaults claim neither `SHARED` nor `PERSISTENT`, including L4,
  whose nominal contract is persistent.
- `cargo deny` runs with `all-features = true`, so it audits the graph CI actually
  builds rather than the default-feature one.

### Verification

Twenty mandatory gates execute in ~50s on a warm cache, plus deferred `loom`,
`performance` and `soak` gates. `xtask` itself had no tests, so the logic deciding
which gates run — tool probing, argv construction, and the verdict on an exit code
— was unverified; it is now under test by the `xtask_unit` gate. Seven tests that
could not fail have been rewritten,
among them `test_tier_recovery` (which called `set_healthy(true)` — already the
default, so no failure was ever injected) and `test_strict_policy_denies_anonymous_writes`
(which never issued an anonymous write).

## v0.3.0 — 2026-10-02

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
