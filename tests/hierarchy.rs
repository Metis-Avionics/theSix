#![allow(unused_imports)]
mod common;

use std::sync::Arc;

use thesix::{Cachelito, DefaultPolicy, EntryState, KeyRef, MemoryPool, TierId, TierRegistry};

use common::{make_manager, test_ctx};

#[tokio::test]
async fn test_basic_get_set() {
    let manager = make_manager(DefaultPolicy);
    let key = "test-key".to_string();
    manager
        .set(&key, "test-value".to_string(), &test_ctx())
        .await
        .unwrap();
    let result = manager.get(&key, &test_ctx()).await.unwrap();
    assert_eq!(result, Some("test-value".to_string()));
}

#[tokio::test]
async fn test_tier_traversal() {
    let manager = make_manager(DefaultPolicy);
    let key = "tier-key".to_string();

    manager
        .set(&key, "tier-value".to_string(), &test_ctx())
        .await
        .unwrap();

    let result = manager.get(&key, &test_ctx()).await.unwrap();
    assert_eq!(result, Some("tier-value".to_string()));

    // Address the control plane the way the manager does, tenant included:
    // reading it with a bare application key silently finds nothing, and the
    // assertion below would then be asserting against an empty entry.
    let ctx = test_ctx();
    let snapshot = manager
        .cachelito()
        .peek(&testkit::framed_key(&ctx, "tier-key"))
        .expect("peek");
    assert_eq!(snapshot.state, EntryState::Ready);
    assert!(
        matches!(snapshot.tier, TierId::L1),
        "the entry landed on {} rather than L1",
        snapshot.tier
    );
}
