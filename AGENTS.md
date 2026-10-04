# AGENTS.md — theSix

Derived from [`theSix.toml`](./theSix.toml), which is the source of truth. If
this file and the contract disagree, the contract wins and
`cargo xtask contract` is the check that should have caught it.

## What this crate is

A six-tier cache whose *architectural* abstraction is the **continuity
contract** between consumers and heterogeneous storage. L0–L6 are
replaceable infrastructure; the contract is not.

```text
Consumer -> continuity/policy plane -> heterogeneous storage topology
```

Consumers never name a rung and never see a backend type.

## Build & test

```bash
just                # every mandatory gate, in contract order (~50s warm)
just quick          # fmt + contract + check + fast layers
just list           # what runs and why
just plan           # print every gate's argv without running it
just perf           # percentile / boundedness gates
just soak           # endurance gates
just loom           # exhaustive control-plane interleavings
just ready          # is this branch mergeable? (open blockers in bugs.toml)
just <gate>         # any single gate by name
```

`cargo xtask` is the entry point. It exits `3` for "could not verify" so an
unavailable tool is never mistaken for a passing gate.

Individual commands:

```bash
cargo build
cargo nextest run --all-features              # all tests
cargo nextest run --all-features --test negative
cargo clippy --all-targets --all-features -- -D warnings
```

## Gates (the contract is authoritative)

```bash
cargo xtask contract        # validate theSix.toml against the runner + checkout
cargo xtask gates           # the mandatory pass
cargo xtask run <name>      # one gate
cargo xtask deferred        # what is excluded, and why
cargo xtask layers          # test-target -> layer mapping
cargo xtask toolchain       # which accelerations are active
cargo bench --all-features  # criterion benchmarks
```

`merge_readiness` is the one gate that judges the branch rather than the change.
It reads `bugs.toml` and fails while any `blocks_merge` finding is open. The
expected finding counts live in `theSix.toml`, not in the tracker, so a finding
cannot be deleted — or downgraded to dodge the check — without editing the
contract. A finding clears only by being `resolved` with a `rationale`, or
`accepted-risk` with both `accepted_by` and `rationale`.

Order matters: fmt → contract → authorship → xtask_unit → check → clippy → doc →
tests → doctest → deny → machete → package → merge_readiness. `loom`,
`performance`, `soak` and `fuzz` are deferred.

`authorship` is the second-cheapest gate and runs early because it judges the
change under review, not the branch. It reads every commit message in the
repository — across all refs, not just `HEAD` — and fails on a trailer key listed
in `[verification.attribution].forbidden_trailers`. Two properties are load-bearing:

* the rule is on the trailer **key**, and the keys live in the contract, so the
  gate never has to contain an attribution in order to forbid one;
* every way of *not* being able to check — no `git`, not a work tree, a shallow
  clone, an empty key list — reports `Unavailable` and exits `3`. GitHub's default
  checkout is depth 1, which would otherwise make this a pass over the tip commit.

The CI job for it sets `fetch-depth: 0`. The gate detects a shallow clone, but
detecting the wrong clone is not a substitute for asking for the right one.

## Architecture

### Control plane — `src/control/cachelito.rs`

A pre-allocated sharded slot map holding entry state, generation, population
ownership and a **commit intent**. No `DashMap` (TETANUS Rule 3): everything is
allocated at init.

* Every method is **synchronous** and returns an owned `ControlSnapshot`. That is
  what makes "no guard across `.await`" structural rather than a convention.
* `peek` observes without claiming. `acquire` claims. Using `acquire` for a
  read-only operation wedges the entry — that was a real bug in `get` and
  `exists`.
* `prepare` / `commit` / `abort` are the two-phase commit. The intent holds a key
  hash, a target rung, a generation and a kind — **never a payload**.
* `abort` restores the state the intent interrupted, and declines if a `commit`
  already won.
* `bump_generation` does the read-modify-write under the shard lock. A caller
  must not compute `snapshot.generation + 1` itself.

### Data plane — `src/tier/`

`CacheTier` is `async` (`#[async_trait]`, boxed futures for dyn-compatibility)
because real backends are I/O. Implementations store and retrieve; they decide
nothing.

* Every value carries a `ContentDigest`; a mismatch is `CacheError::Corrupted`.
* `CacheError::WriteIndeterminate` is the only error that implies the rung may hold
  an uncommitted value, and it is the only one the manager compensates for by
  removing the key. A definite failure provably stored nothing, so compensating for
  it would delete the value already there. A tier capability flag cannot make this
  distinction — `WriteFailure` and `PartialWrite` share a tier and need opposite
  handling — so the tier reports it per operation instead.
* Slots are identified by a 128-bit `KeyFingerprint` over the full key, so a
  placement collision cannot alias two keys. `Placement` is injectable so a test
  can force the collision and demonstrate the property.
* `find_slot` scans the whole table rather than stopping at a hole: open
  addressing plus deletion orphans entries otherwise.
* `capability()` is a **default method** that reports pessimistically. A tier
  that can do better overrides it.

### Manager — `src/manager.rs`

`CacheManager<K, V, P>` enforces authentication, delegates tier selection to
policy, and coordinates through `Cachelito`.

* `bound_tier(id) -> Option<..>` is the only rung accessor. There is no
  substitution: an unbound rung returns `None`, never another rung's tier.
* `nearest_bound_rung(wanted)` resolves a *policy choice* to the best bound rung
  at or below it. That is routing, not substitution.
* `set` walks down the ladder on any rung-level failure (full, unavailable,
  timed out, corrupt) and retries a lost commit race. It returns an error only
  when every rung below refuses.
* `wait_for_population` registers with `Notified::enable()` **before** deciding
  to wait, and re-reads afterwards. `notify` is `notify_waiters`; the two halves
  pair to close the lost-wakeup window without stranding other waiters.
* A `PopulationGuard` releases the claim on `Drop`, so cancellation cannot wedge
  a key.

### Policy — `src/policy.rs`

`cache_ladder()` is the single iteration source for every rung scan. The
fail-open fallback, promotion and demotion are all bounded by
`LAST_CACHE_TIER`, so a fallback can never surface authority data.

### Capability / authority / continuity

* `src/capability.rs` — `CapabilityFlags` × `OperationalState` ×
  `DurabilityClass`. Three axes because the properties are not mutually
  exclusive.
* Authority is a **configured role** (`CacheManager::authority_tier`), stamped
  onto the capability report rather than inferred from a tier number.
* `src/continuity.rs` — `ContinuityState`, `RecoveryDirection`,
  `RecoveryOutcome`, `RecoveryReport`. A `Write` intent aborts on recovery. A
  `Move` does **not** complete forward: that needs the key bytes and the control
  plane keeps only a hash, so `for_kind(Move)` returns `ExternalReconciliation`
  and the sweep reports it in its own counter and leaves the intent intact.
  `tests/contract` asserts the declared clause against `for_kind`, because a
  clause nothing checks is a promise the runtime does not have to keep.
* `src/integrity.rs` — digests and fingerprints. The digest is explicitly
  **non-cryptographic**: it detects accidental corruption, not tampering.
* `src/telemetry.rs` — `OperationRecord` with the eleven contract fields. No
  payload; keys appear only as a `KeyIdentity` digest plus a length.

### Keys and tenants

`frame_tenant_key` folds the tenant into the key with an explicit `0x00`
separator. Concatenation would make tenant `ab` + key `c` indistinguishable from
tenant `a` + key `bc`. An oversized frame is **rejected, not truncated**.

`IdentityContext::tenant` used to be read by zero lines of the crate; it is now
part of the key. **This invalidates every existing cached key** — see the 2.0.0
release notes.

## Test layers

| Layer | Target | Demonstrates |
|---|---|---|
| contract | `tests/contract` | The TOML is load-bearing |
| unit | `integration` `hierarchy` `policy` `stampede` | Core behaviour |
| negative | `negative` | 19 failure modes, each by resulting state |
| fault_injection | `fault_injection` | 11 faults, each proven to fire |
| property | `property` | 9 invariants, randomised with shrinking |
| concurrency | `concurrency` `await_safety` `sharding` `loom` | Adversarial races |
| capability | `capability` `l6_authority` | No false claims |
| recovery | `recovery` | Idempotent, repeatable recovery |
| durability | `durability` | Only proven claims are `Verified` |
| security | `security` | No cross-tenant/key access; no payload leaks |
| performance | `performance` | Percentiles, shard independence, boundedness |
| soak | `soak` | Endurance, capacity, isolation at volume |
| backends | `backends` `oxigraph_backend` | Real backends |

`xtask` is under test rather than trusted: `xtask_unit` covers tool probing, argv
construction and the merge-readiness check, because the runner decides what passes.

`testkit` is a dev-only crate holding the harness: `FaultyTier` with its
`FaultLedger`, `RecordingTier`, `HangingTier`, `framed_key`, and the coverage
registries. Use it rather than duplicating a manager builder.

### Anti-vacuity — non-negotiable

* A test that injects a fault asserts the **ledger** recorded it.
* A test that claims a stall asserts `HangingTier::reached() > 0`.
* A test that reaches for the control plane uses `testkit::framed_key`; a raw
  application key silently addresses nothing.
* A test that seeds an entry must `commit` it. A merely `prepare`d entry reads as
  `Prepared` and never routes to a rung, so injected read faults will not fire.
* `testkit::coverage` registries are compared for **equality** against the
  contract, so dropping a required case fails `cargo xtask contract`.

## Repository config

* Edition **2024**, MSRV **1.98**. `#![deny(warnings)]` and
  `#![forbid(unsafe_code)]` at the crate root.
* Build profiles use `debug = 0` and `opt-level = 1` for dependencies: the gate
  matrix ran out of disk with debuginfo on (18GB of artifacts).
* `.cargo/config.toml` sets sccache; the gate runner adds `clang` + `mold` when
  both are present.
* `loom-tests` is a cargo **feature**, not `--cfg loom`: rustflags are part of
  the sccache key, so switching them invalidates every dependency build.
* `fuzz/` is an excluded workspace: it needs nightly and libFuzzer, so it cannot
  run under the MSRV gates. CI runs it as an advisory job.
* `living.toml` tracks handover state.