# theSix Skeleton & Specs Plan

## Context

**Project:** theSix — policy-driven six-tier cache orchestration (Rust library)  
**Repos:**  
- `Metis-Avionics/theSix` → the cache orchestration library (this repo)  
- `Metis-Avionics/theDAF` → downstream consumer (application layer)  
- `Metis-Avionics/theMQL` → downstream consumer (application layer)

## Resolved Design Decisions

| Decision | Resolution |
|---|---|
| Authn boundary | **CacheManager gate (Option B):** unauthenticated requests rejected before any state is touched → `CacheError::Unauthenticated` |
| Authz boundary | **Policy engine rule (Option C):** identity is a first-class input to `select()`; `PolicyDecision.authorized` determines access; denied → `CacheError::Unauthorized` |
| Fail-open/closed | **Policy engine decides per-request (Option B):** `PolicyDecision.fail_mode` is `Open` (bypass tier, try fallback) or `Closed` (fail fast); circuit breaker state lives in Cachelito; policy interprets it |
| theDAF / theMQL | Downstream application subsystems that consume theSix as a library; not part of this crate |

## Request Lifecycle (authoritative)

```
Application
  │  supplies IdentityContext + key + operation
  ▼
CacheManager
  │  1. Authn gate: identity present? → Unauthenticated if absent
  │  2. Resolve policy: policy.select(request, state, identity)
  │  3. Authz check: decision.authorized? → Unauthorized if false
  │  4. Read Cachelito: entry state for key
  │  5a. InFlight → wait (single-flight)
  │  5b. Ready   → return cached value from tier
  │  5c. Absent/Stale → become population owner
  │       ├─ fetch from origin / compute
  │       ├─ write to policy-selected tier
  │       ├─ on tier failure:
  │       │   Open  → try next-healthiest tier
  │       │   Closed → propagate TierUnavailable
  │       └─ publish result to Cachelito
  ▼
Six-Tier Data Plane
```

## Specs to Create

Create the following files under `specs/`. Content is derived from the README inline specs, augmented with authn/authz and fail-open/closed.

### `specs/thesix.toml`

```toml
[system]
name = "theSix"
version = "0.1"
description = "Policy-driven six-tier cache orchestration for Rust"
model = "control-plane-over-data-plane"

[architecture]
tier_count = 6
application_selects_tier = false
manager_selects_tier = true
policy_selects_tier = true
control_plane = "cachelito"

[principles]
separate_control_plane = true
separate_data_plane = true
policy_driven_routing = true
opaque_tier_topology = true
single_flight_population = true
generation_based_invalidation = true
no_io_while_holding_control_guard = true
authn_at_manager = true
authz_at_policy = true
fail_mode_at_policy = true

[guarantees]
concurrent_readers = true
tier_selection_is_deterministic = true
tier_failure_is_isolated = true
stampede_detection = true
population_coordination = true
authz_never_reaches_data_plane = true

[non_goals]
distributed_consensus = false
general_persistence_layer = false
application_business_logic = false
global_cache_coherence = false
```

### `specs/cache_manager.toml`

```toml
[manager]
name = "CacheManager"

[identity]
context_type = "IdentityContext"
supplied_by_application = true
authn_gate = "pre-policy"
authn_failure = "Unauthenticated"

[operations]
get = true
get_or_fetch = true
set = true
invalidate = true
remove = true
refresh = true
exists = true

[operations.promotion]
enabled = true
policy_controlled = true

[operations.demote]
enabled = true
policy_controlled = true

[routing]
application_direct_tier_selection = false
manager_resolves_policy = true
manager_resolves_tier = true
identity_flows_to_policy = true

[execution]
async = true
never_hold_control_guard_across_io = true

[errors]
tier_failure_propagation = "policy"
population_failure_propagation = "single_flight"
unauthenticated = "Unauthenticated"
unauthorized = "Unauthorized"
```

### `specs/cachelito.toml`

```toml
[control]
name = "Cachelito"
role = "cache-control-plane"

[storage]
structure = "sharded-concurrent-map"
implementation = "DashMap"

[state]
tracks_entry_state = true
tracks_generation = true
tracks_tier = true
tracks_population = true
tracks_expiration = true
tracks_tier_health = true

[entry_states]
values = [
    "absent",
    "ready",
    "stale",
    "in_flight",
    "failed"
]

[circuit_breaker]
per_tier = true
consecutive_failure_count = true
last_failure_timestamp = true
health_score = true
state_does_not_decide_fail_mode = true

[coordination]
single_flight = true
reader_coalescing = true
writer_coordination = true

[concurrency]
multiple_readers = true
sharded_state = true
global_lock = false
io_under_guard = false

[invariants]
control_state_does_not_store_payload = true
control_guard_must_not_cross_await = true
control_guard_must_not_cross_io = true
circuit_breaker_state_is_advisory = true
```

### `specs/policy.toml`

```toml
[policy]
name = "default"
mode = "deterministic"

[selection]
latency = true
capacity = true
consistency = true
availability = true
entry_size = true
ttl = true
cost = true
identity = true

[identity_inputs]
principal = true
roles = true
tenant = true

[resolution]
input = [
    "operation",
    "key_metadata",
    "entry_metadata",
    "cache_state",
    "tier_health",
    "identity"
]
output = [
    "authorized",
    "selected_tier",
    "operation",
    "population_strategy",
    "fail_mode"
]

[precedence]
explicit_policy = 1
tier_health = 2
consistency_requirement = 3
authz_deny = 4
latency_requirement = 5
capacity_requirement = 6
default_tier = 7

[fail_mode]
open = "bypass_failed_tier_try_next"
closed = "fail_fast_propagate_error"
```

### `specs/tiers.toml`

```toml
[tiers]
count = 6

[tier.l0]
role = "request"
scope = "request"
persistent = false
shared = false

[tier.l1]
role = "hot-local"
scope = "process"
persistent = false
shared = false

[tier.l2]
role = "local"
scope = "process"
persistent = false
shared = false

[tier.l3]
role = "distributed"
scope = "cluster"
persistent = false
shared = true

[tier.l4]
role = "persistent"
scope = "host"
persistent = true
shared = false

[tier.l5]
role = "origin-fallback"
scope = "external"
persistent = false
shared = true

[routing]
manager_only = true
policy_only = true

[health]
health_checks = true
failure_isolation = true
circuit_breaker_per_tier = true
fail_mode_decided_by_policy = true
```

### `specs/stampede.toml`

```toml
[stampede]
enabled = true
strategy = "single-flight"

[ownership]
one_population_owner = true
waiters_join_existing_population = true

[timeouts]
population_timeout = "5s"
wait_timeout = "5s"

[failure]
owner_failure_releases_state = true
waiters_receive_population_error = true
retry_after_failure = true

[retry]
enabled = true
max_attempts = 3
backoff = "exponential"

[invariants]
duplicate_population_for_same_key = false
stale_infinite_inflight_state = false
owner_loss_recoverable = true
```

## Implementation Skeleton

Ordered task list for the implementation-capable agent:

### Phase 1: Project scaffold

1. Create `Cargo.toml` with dependencies: `dashmap`, `tokio` (features: `sync`, `time`, `macros`, `rt-multi-thread`), `thiserror`, optional `redis`, `moka`, `sled`
2. Create `src/lib.rs` with crate-level documentation and module declarations
3. Create `src/error.rs` — `CacheError` enum covering: `Unauthenticated`, `Unauthorized`, `TierUnavailable`, `PolicyDenied`, `Miss`, `PopulationFailed`, `Timeout`, `Cancelled`, `StaleGeneration`, `SerializationFailed`, `ConfigurationError`
4. Create `src/entry.rs` — `EntryState` (Absent, Ready, Stale, InFlight, Failed), `CacheEntry<V>` (value, generation, expiration), `Generation` (newtype u64)

### Phase 2: Tier abstraction

5. Create `src/tier/mod.rs` — `TierId` (L0–L5), `TierRegistry`
6. Create `src/tier/trait.rs` — `CacheTier` trait: `get`, `set`, `remove`, `contains`, `health`, `name`
7. Create stub tier modules: `src/tier/l0.rs` through `src/tier/l5.rs` (in-memory HashMap stubs for L0–L2; trait-ready signatures for L3–L5)

### Phase 3: Cachelito control plane

8. Create `src/control/mod.rs`
9. Create `src/control/cachelito.rs` — sharded DashMap storing control entries (no payload); entry tracks: `state`, `generation`, `tier`, `population_owner`, `population_timestamp`, `tier_health` (consecutive failures, last failure); methods: `acquire`, `publish`, `fail`, `release`, `health`

### Phase 4: Policy engine

10. Create `src/policy.rs` — `CachePolicy` trait with `select(&self, request, state, identity) -> PolicyDecision`; `PolicyDecision` struct: `authorized: bool`, `tier: TierId`, `operation: CacheOperation`, `population: PopulationStrategy`, `fail_mode: FailMode`; `DefaultPolicy` implementation
11. Define `IdentityContext` (principal, roles, tenant) — placed in `src/identity.rs` or inline in manager

### Phase 5: CacheManager

12. Create `src/manager.rs` — `CacheManager<K, V, P>` struct holding: policy, cachelito, tier_registry, config; `get`, `set`, `invalidate`, `remove`, `get_or_fetch` methods; authn gate at entry; policy resolution with identity; fail-open tier fallback loop; no guard held across await
13. Create `src/lib.rs` exports: `CacheManager`, `CacheError`, `CacheTier`, `CachePolicy`, `PolicyDecision`, `IdentityContext`, `TierId`, `EntryState`, `FailMode`, `PopulationStrategy`

### Phase 6: Specs files

14. Write all six `specs/*.toml` files as specified above
15. Update `README.md` references to point to spec files

### Phase 7: Tests (skeleton)

16. Create `tests/hierarchy.rs` — basic get/set across tiers
17. Create `tests/concurrency.rs` — concurrent readers, writer/readers, unrelated keys
18. Create `tests/policy.rs` — policy selection, authz deny, fail-mode routing
19. Create `tests/stampede.rs` — single-flight, 100 concurrent misses, owner failure, waiter timeout
20. Create `tests/integration.rs` — full six-tier integration with stubs

### Phase 8: Benchmarks

21. Create `benches/cache_operations.rs` — uncontended get, concurrent get, same-key contention, policy eval, cachelito lookup, single-flight coordination

## Validation

Run in order:
1. `cargo fmt --check`
2. `cargo check`
3. `cargo test`
4. `cargo clippy --all-targets --all-features -- -D warnings`
5. `cargo doc --no-deps`
6. `cargo package --list`
7. `cargo publish --dry-run`

## Risks

| Risk | Mitigation |
|---|---|
| Policy engine coupling to identity types | IdentityContext is a generic input; policy trait is generic over identity type |
| Fail-open fallback complexity | Initial impl: linear tier scan on failure; later: priority queue from tier health |
| DashMap guard across await | All Cachelito lookups clone state out of guard before async work |
| Spec/impl drift | Specs are the source of truth; implementation must match |

## Out of Scope

- Real tier backend implementations (Redis, Moka, Sled) — stubs only
- Distributed coordination (cross-process Cachelito)
- Metrics/monitoring subsystem (`metrics.rs`)
- TheDAF / theMQL integration code
