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

    let snapshot = manager
        .cachelito()
        .acquire(b"tier-key", TierId::L0)
        .unwrap();
    assert_eq!(snapshot.state, EntryState::Ready);
    assert!(matches!(snapshot.tier, TierId::L1));
}
