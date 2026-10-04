# theSix

[![CI](https://github.com/Metis-Avionics/theSix/actions/workflows/ci.yml/badge.svg)](https://github.com/Metis-Avionics/theSix/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/thesix.svg)](https://crates.io/crates/thesix)
[![docs.rs](https://docs.rs/thesix/badge.svg)](https://docs.rs/thesix)

Policy-driven six-tier cache orchestration for Rust, built around an explicit
**high-performance availability (HPA) data-continuity contract**.

> **The contract is the source of truth.** [`theSix.toml`](./theSix.toml) declares
> the architecture's invariants, and this file, the crate docs and `AGENTS.md`
> derive from it. Where prose and contract could disagree, the contract wins — and
> `cargo xtask contract` fails the build if they do.

## The boundary

```text
                    Consumer
                        │
                        ▼
        theSix continuity / policy plane
        locality · availability · performance
        consistency · durability · recovery
                        │
    ┌───────────────────┼───────────────────┐
    ▼                   ▼                   ▼
Performance         Continuity           Security
& locality          & recovery           CIA
    └───────────────────┼───────────────────┘
                        ▼
        Heterogeneous storage / cache / authority
```

Consumers depend on the **continuity contract**, not on a backend. L0–L6 are
replaceable infrastructure: the rung a value lands on is decided by policy, and
authority is configured rather than inferred from a tier number.

## Quickstart

```rust
use std::sync::Arc;
use thesix::{CacheContext, CacheManager, CacheTier, Cachelito, DefaultPolicy,
             IdentityContext, MemoryPool, TierRegistry};

# async fn example() {
let tiers: Vec<Arc<dyn CacheTier<String>>> = vec![
    Arc::new(thesix::L0Stub::<String>::new()),
    Arc::new(thesix::L1Stub::<String>::new()),
    Arc::new(thesix::L2Stub::<String>::new()),
    Arc::new(thesix::L3Stub::<String>::new()),
    Arc::new(thesix::L4Stub::<String>::new()),
    Arc::new(thesix::L5Stub::<String>::new()),
];
let pool = MemoryPool::<String>::new(1024)?;

let manager: CacheManager<String, String, DefaultPolicy> = CacheManager::new(
    DefaultPolicy, Cachelito::new(), TierRegistry::new(), tiers, pool,
);

// Identity carries the tenant. The tenant is part of the key, so two tenants
// using the same key never share an entry.
let ctx = CacheContext::new(IdentityContext::new(
    "alice".into(), vec!["reader".into()], "tenant-1".into(),
));

let key = "order-42".to_string();
let value = manager
    .get_or_fetch(&key, &ctx, || async { Ok("payload".to_string()) })
    .await?;
# Ok::<(), thesix::CacheError>(())
# }
```

Application code never names a rung. Every operation takes `(key, context)` and
nothing else.

## What the crate guarantees

| Property | Mechanism |
|---|---|
| **Atomicity** | Two-phase commit with a control-plane intent record. `prepare → write → commit`; a crash leaves an intent, never a half-visible value. |
| **Isolation** | Single-flight population ownership; no control guard is ever held across an `.await`. |
| **Durability honesty** | `DurabilityClass` is `Volatile`, `Delegated`, or `Verified`. Only a test that drops a store and reads it back may grant `Verified`. |
| **Integrity** | Every stored value carries a content digest. A damaged record is reported as `Corrupted`, never served. |
| **Tenant isolation** | The tenant is framed into the key, so a cross-tenant read finds nothing rather than finding a neighbour's value. |
| **Capability honesty** | `CapabilityFlags` × `OperationalState` × `DurabilityClass`. An unbound rung is reported `Unbound`, not silently replaced by another. |
| **Bounded control plane** | Every rung-scanning path is bounded by `LAST_CACHE_TIER`, so a fallback can never surface authority data. |

### Atomicity, concretely

```text
prepare(key, generation, rung)  →  entry is Prepared; reads see a miss
write to the rung               →  no payload anywhere in the control plane
commit(token)                   →  Ready, intent cleared, waiters notified
abort(token)                    →  restores the interrupted state
```

Recovery resolves an outstanding intent by kind: a **write** aborts (the value is
reproducible), a **move** completes forward (aborting it would discard an
already-committed value). Both directions are idempotent, and a move that cannot
be resolved without the key is *reported* rather than silently skipped.

## Verification

Every gate is declared in `theSix.toml` and executed from it.

```bash
just                 # the mandatory gates, in contract order
just gates           # the same, spelled out
just quick           # fmt + contract + check + the fast layers
just list            # what runs, and why
just perf            # percentile and boundedness gates
just soak            # endurance gates
just ready           # is this branch mergeable?
just loom            # exhaustive control-plane interleavings
just plan            # print every gate's argv without running it
```

`cargo xtask` is the entry point. It refuses to run a gate the contract does not
declare, refuses to start if a declared gate has no execution strategy, and
refuses to report success for a gate whose tooling is missing — it exits `3` for
"could not verify" rather than `0` for "verified".

### Test layers

| Layer | Target | What it demonstrates |
|---|---|---|
| contract | `tests/contract` | The TOML is load-bearing: version, rung count, registries, layer bindings. |
| unit | `integration`, `hierarchy`, `policy`, `stampede` | Round-trips, TTL, generation rejection, single-flight, authz. |
| negative | `negative` | All 19 required failure modes, each verified by its resulting state. |
| fault_injection | `fault_injection` | All 11 faults, each proven to have fired via its ledger. |
| property | `property` | The 9 named invariants over randomised operation sequences. |
| concurrency | `concurrency`, `await_safety`, `sharding`, `loom` | Adversarial races with forced interleavings. |
| capability | `capability`, `l6_authority` | No rung claims a backend, authority, or durability it lacks. |
| recovery | `recovery` | The full lifecycle; idempotent, repeatable recovery. |
| durability | `durability` | Drop-and-reopen; only proven claims are `Verified`. |
| security | `security` | Cross-tenant/key access refused; no payload in errors or telemetry. |
| performance | `performance` | Percentiles, shard independence, bounded control plane. |
| soak | `soak` | Slot-table pressure, tenant isolation at volume, recovery scaling. |
| backends | `backends`, `oxigraph_backend` | Real backends behind feature gates. |

### Anti-vacuity

A test that cannot prove its own fault fired is decoration. Three mechanisms
enforce this:

1. **`FaultLedger`** counts every fault activation. Every negative and
   fault-injection test asserts its fault actually fired.
2. **Registries** in `testkit::coverage` are compared for *equality* against the
   contract's lists, so a required case cannot be dropped from the suite without
   the contract gate failing.
3. **Capability assertions** check a claim against something observable. A rung
   reporting `Healthy` must store; a rung reporting `Persistent` must survive a
   restart; a rung reporting `authoritative` must be the configured authority rung.

### Build accelerations

`.cargo/config.toml` uses sccache; the gate runner additionally selects
`clang` + `mold` when both are installed, and `cargo xtask toolchain` reports what
it found. The crate itself has no build-script requirement and no `unsafe`.

## Backends

| Rung | Role | In-memory default | Real backend |
|---|---|---|---|
| L0 | request-local | yes | — |
| L1 | hot-local | yes | — |
| L2 | local | yes | — |
| L3 | distributed | fallback | `redis` (feature) |
| L4 | persistent | fallback | `sled` (feature) |
| L5 | origin / graph | fallback | `oxigraph` (feature) |
| L6 | **authority** | unbound | application-defined |

The L3–L5 defaults store values so the ladder works out of the box, and report
`BackendKind::InMemoryFallback` — they claim neither `SHARED` nor `PERSISTENT`,
because they are process-local and saying otherwise would be a durability lie.
`CacheManager::capabilities()` tells you what every rung is *actually* bound to.

## License

MIT.