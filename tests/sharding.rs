//! The in-memory tier stubs are sharded, and the sharding is real.
//!
//! The coarse lock these replace was one `Mutex` per tier: every concurrent
//! access to L0 serialised on it. A test that only checked "it still stores
//! values" would pass on the old single-lock code too, so the assertions here
//! are about distribution and independence, not correctness of the round-trip.

#![allow(dead_code)]

use std::collections::HashSet;
use std::hash::Hasher;

use thesix::CacheError;
use thesix::KeyRef;
use thesix::tier::sharded_stub::{DEFAULT_SHARDS, ShardedTierStub};

fn key(s: &str) -> KeyRef<'_> {
    KeyRef::from(s.as_bytes())
}

/// Every shard must be reachable. An earlier version rounded the shard count
/// DOWN to fit the mask, which with n=3 and mask=2 made shard 1 permanently
/// unreachable - indices could only ever be 0 or 2. This walks many keys and
/// requires the observed shard count to match the requested one.
#[test]
fn shard_count_rounds_up_so_every_shard_is_reachable() {
    for requested in [1usize, 2, 3, 4, 5, 6, 7, 8] {
        let stub: ShardedTierStub<String> =
            ShardedTierStub::with_shards(requested, 256).expect("valid configuration");

        let mut observed: HashSet<usize> = HashSet::new();
        for i in 0..128u32 {
            let k = format!("key-{i}");
            let kr = key(&k);
            stub.set(&kr, i.to_string(), None).expect("set");
        }
        // Recompute placement the same way the stub does, and count distinct
        // shards actually selected across a large key space.
        for i in 0..128u32 {
            let k = format!("key-{i}");
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&key(&k), &mut hasher);
            observed.insert((hasher.finish() as usize) & (stub.shard_mask()));
        }

        let expected = requested.max(1).next_power_of_two();
        assert_eq!(
            observed.len(),
            expected,
            "with {requested} shards, {expected} distinct shards should be reachable, saw {}",
            observed.len()
        );
    }
}

/// Concurrent writes to distinct keys must all land. On a single mutex this
/// still passes, so it is paired with the distribution test above rather than
/// trusted alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_writes_to_distinct_keys_all_land() {
    let stub = std::sync::Arc::new(
        ShardedTierStub::<String>::with_shards(DEFAULT_SHARDS, 64).expect("valid configuration"),
    );

    let mut handles = Vec::new();
    for i in 0..64u32 {
        let stub = stub.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            let k = format!("concurrent-{i}");
            stub.set(&key(&k), i.to_string(), None).expect("set");
        }));
    }
    for h in handles {
        h.await.expect("no thread panicked");
    }

    for i in 0..64u32 {
        let k = format!("concurrent-{i}");
        assert_eq!(
            stub.get(&key(&k)).expect("get").as_deref(),
            Some(i.to_string().as_str()),
            "key {k} was lost"
        );
    }
}

/// Overwrites on one key must be serialised: the last write wins, and no read
/// observes a torn value.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_overwrites_of_one_key_are_serialised() {
    let stub = std::sync::Arc::new(
        ShardedTierStub::<String>::with_shards(DEFAULT_SHARDS, 64).expect("valid configuration"),
    );
    stub.set(&key("hot"), "0".to_string(), None).expect("seed");

    let mut handles = Vec::new();
    for i in 1..64u32 {
        let stub = stub.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            stub.set(&key("hot"), i.to_string(), None).expect("set");
        }));
    }
    for h in handles {
        h.await.expect("no thread panicked");
    }

    // Serialisation means the read-back is exactly one of the written values -
    // never a blend of two. A torn read would not parse as any single one.
    let final_value = stub
        .get(&key("hot"))
        .expect("get")
        .expect("a value is present");
    let parsed: u32 = final_value
        .parse()
        .unwrap_or_else(|_| panic!("read back a torn value {final_value:?}"));
    assert!(
        (1..64).contains(&parsed),
        "read back {parsed}, which was never written"
    );
}

/// TTL expiry still works through the shard indirection.
#[tokio::test]
async fn ttl_expiry_survives_sharding() {
    let stub: ShardedTierStub<String> =
        ShardedTierStub::with_shards(DEFAULT_SHARDS, 16).expect("valid configuration");
    stub.set(
        &key("ttl"),
        "v".to_string(),
        Some(std::time::Duration::from_millis(20)),
    )
    .expect("set");
    assert_eq!(stub.get(&key("ttl")).expect("get").as_deref(), Some("v"));
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert!(
        stub.get(&key("ttl")).expect("get").is_none(),
        "an expired entry survived the shard round-trip"
    );
}

/// A full shard reports capacity exhaustion rather than silently dropping.
#[test]
fn a_full_shard_reports_capacity_exhausted() {
    let stub: ShardedTierStub<String> = ShardedTierStub::with_shards(1, 2).expect("valid");
    stub.set(&key("a"), "1".to_string(), None).expect("set");
    stub.set(&key("b"), "2".to_string(), None).expect("set");
    let third = stub.set(&key("c"), "3".to_string(), None);
    assert!(
        matches!(third, Err(CacheError::CapacityExhausted)),
        "a full shard must report CapacityExhausted, got {third:?}"
    );
}

/// A poisoned shard must surface as an error rather than being papered over.
///
/// The `poisoned` handler exists for that, but it cannot be exercised through the
/// public API: every guard is scoped inside `get`/`set`/`remove`/`contains` and
/// released before returning, so there is no window in which a caller can panic
/// while holding one. A first draft of this test panicked from another thread
/// after `set` had already returned, which poisons nothing, and it correctly
/// read back `Ok(Some("v"))`.
///
/// Staging a genuine poison needs a test-only hook that holds a guard across a
/// caller-supplied closure. That is worth adding only if a real poisoning path
/// is found, so rather than assert something unreachable, this records why the
/// handler is currently unexercised.
#[test]
fn a_poisoned_shard_is_not_reachable_through_the_public_api() {
    let stub: ShardedTierStub<String> =
        ShardedTierStub::with_shards(DEFAULT_SHARDS, 16).expect("valid");
    assert!(stub.set(&key("k"), "v".to_string(), None).is_ok());
    // No guard outlives the call, so a later panic elsewhere cannot poison the
    // shard and this read must succeed.
    std::thread::spawn(|| panic!("unrelated panic in another thread"))
        .join()
        .ok();
    assert_eq!(
        stub.get(&key("k")).expect("get").as_deref(),
        Some("v"),
        "an unrelated panic must not surface as a poisoned-shard error"
    );
}
