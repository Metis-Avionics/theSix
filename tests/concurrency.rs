#![allow(unused_imports)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use thesix::{Cachelito, DefaultPolicy, KeyRef, MemoryPool, TierRegistry};

use common::{make_manager, make_manager_with_timeout, test_ctx};

#[tokio::test]
async fn test_concurrent_readers() {
    let manager = make_manager(DefaultPolicy);
    let key = "shared-key".to_string();

    manager
        .set(&key, "shared-value".to_string(), &test_ctx())
        .await
        .unwrap();

    let mut handles = vec![];
    for _ in 0..50 {
        let m = Arc::clone(&manager);
        let k = key.clone();
        handles.push(tokio::spawn(async move {
            m.get(&k, &test_ctx()).await.unwrap()
        }));
    }

    for handle in handles {
        let result = handle.await.unwrap();
        assert_eq!(result, Some("shared-value".to_string()));
    }
}

#[tokio::test]
async fn test_concurrent_writer_readers() {
    let manager = make_manager(DefaultPolicy);
    let key = "rw-key".to_string();

    let mut handles = vec![];

    for i in 0..10 {
        let m = Arc::clone(&manager);
        let k = key.clone();
        handles.push(tokio::spawn(async move {
            if i % 3 == 0 {
                m.set(&k, format!("value-{}", i), &test_ctx())
                    .await
                    .unwrap();
            } else {
                let _ = m.get(&k, &test_ctx()).await;
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_concurrent_unrelated_keys() {
    let manager = make_manager(DefaultPolicy);

    let mut handles = vec![];
    for i in 0..20 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            let key = format!("key-{}", i);
            m.set(&key, i.to_string(), &test_ctx()).await.unwrap();
            let result = m.get(&key, &test_ctx()).await.unwrap();
            assert_eq!(result, Some(i.to_string()));
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn test_shard_contention() {
    let manager = make_manager_with_timeout(DefaultPolicy, Duration::from_secs(2));

    let mut handles = vec![];
    for i in 0..50 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            let key = format!("shard-key-{}", i % 4);
            m.set(&key, i.to_string(), &test_ctx()).await.unwrap();
            let result = m.get(&key, &test_ctx()).await.unwrap();
            assert!(result.is_some());
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
}
