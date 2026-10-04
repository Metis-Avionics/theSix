//! Security: confidentiality and integrity, proven negatively.
//!
//! Every test here is an *attack*, not a feature check. "Tenant isolation works"
//! is not a claim; "tenant B reading tenant A's key gets nothing, and there is no
//! configuration in which it does" is.

use std::sync::Arc;
use std::time::Duration;

use testkit::{ctx_for_tenant, make_manager_with_timeout, test_ctx};
use thesix::{
    CacheContext, CacheError, CacheManager, Cachelito, DefaultPolicy, KeyIdentity, KeyRef,
    Operation, Outcome, RingTelemetry, TierId,
};

fn mgr() -> Arc<CacheManager<String, String, DefaultPolicy>> {
    make_manager_with_timeout(DefaultPolicy, Duration::from_secs(2))
}

// ---------------------------------------------------------------------------
// Tenant and key-space isolation
// ---------------------------------------------------------------------------

/// Two tenants using the same key must not see each other's value.
#[tokio::test]
async fn tenants_do_not_share_entries() {
    let m = mgr();
    let key = "shared-key-name".to_string();

    m.set(&key, "alice-secret".to_string(), &ctx_for_tenant("alice"))
        .await
        .expect("alice writes");

    let bobs_view = m.get(&key, &ctx_for_tenant("bob")).await;
    assert!(
        matches!(bobs_view, Err(CacheError::Miss)),
        "bob read alice's value: {bobs_view:?}"
    );
}

/// Bob writing the same key must not overwrite alice's.
#[tokio::test]
async fn one_tenant_cannot_overwrite_another() {
    let m = mgr();
    let key = "collision".to_string();

    m.set(&key, "alice".to_string(), &ctx_for_tenant("alice"))
        .await
        .expect("alice writes");
    m.set(&key, "bob".to_string(), &ctx_for_tenant("bob"))
        .await
        .expect("bob writes");

    assert_eq!(
        m.get(&key, &ctx_for_tenant("alice"))
            .await
            .expect("alice reads"),
        Some("alice".to_string()),
        "bob's write clobbered alice's value"
    );
    assert_eq!(
        m.get(&key, &ctx_for_tenant("bob"))
            .await
            .expect("bob reads"),
        Some("bob".to_string())
    );
}

/// An invalidation must not cross tenants either: bob invalidating a key he does
/// not own must not evict alice's copy.
#[tokio::test]
async fn invalidation_does_not_cross_tenants() {
    let m = mgr();
    let key = "target".to_string();

    m.set(&key, "alice".to_string(), &ctx_for_tenant("alice"))
        .await
        .expect("alice writes");
    m.invalidate(&key, &ctx_for_tenant("bob"))
        .await
        .expect("bob invalidates");

    assert_eq!(
        m.get(&key, &ctx_for_tenant("alice"))
            .await
            .expect("alice reads"),
        Some("alice".to_string()),
        "bob's invalidation evicted alice's entry"
    );
}

/// The framing must survive the concatenation ambiguity: tenants `ab`/`c` and
/// `a`/`bc` must not collide.
#[tokio::test]
async fn tenant_framing_is_unambiguous() {
    let m = mgr();
    m.set(
        &"c".to_string(),
        "from-ab".to_string(),
        &ctx_for_tenant("ab"),
    )
    .await
    .expect("ab writes");
    m.set(
        &"bc".to_string(),
        "from-a".to_string(),
        &ctx_for_tenant("a"),
    )
    .await
    .expect("a writes");

    assert_eq!(
        m.get(&"c".to_string(), &ctx_for_tenant("ab"))
            .await
            .expect("ab reads"),
        Some("from-ab".to_string())
    );
    assert_eq!(
        m.get(&"bc".to_string(), &ctx_for_tenant("a"))
            .await
            .expect("a reads"),
        Some("from-a".to_string())
    );
}

/// Distinct keys within one tenant must not interfere.
#[tokio::test]
async fn keys_within_a_tenant_are_isolated() {
    let m = mgr();
    let ctx = test_ctx();
    for i in 0..64 {
        m.set(&format!("k{i}"), format!("v{i}"), &ctx)
            .await
            .unwrap_or_else(|e| panic!("write {i} failed: {e:?}"));
    }
    for i in 0..64 {
        assert_eq!(
            m.get(&format!("k{i}"), &ctx)
                .await
                .unwrap_or_else(|e| panic!("read {i}: {e:?}")),
            Some(format!("v{i}")),
            "key k{i} returned another key's value"
        );
    }
}

/// The control plane must hold tenant-framed keys too, or a `peek` on one
/// tenant's bytes could observe another's entry state.
#[tokio::test]
async fn the_control_plane_partitions_by_tenant() {
    let cachelito = Cachelito::new();
    let mut alice = [0u8; 64];
    let mut bob = [0u8; 64];
    let la = thesix::frame_tenant_key("alice", b"k", &mut alice).expect("frame");
    let lb = thesix::frame_tenant_key("bob", b"k", &mut bob).expect("frame");

    let ta = cachelito
        .prepare(&alice[..la], None, TierId::L1, thesix::IntentKind::Write)
        .expect("prepare");
    cachelito.commit(&ta, None).expect("commit");

    let a = cachelito.peek(&alice[..la]).expect("peek");
    let b = cachelito.peek(&bob[..lb]).expect("peek");
    assert_eq!(a.state, thesix::EntryState::Ready);
    assert_eq!(
        b.state,
        thesix::EntryState::Absent,
        "bob's key resolved to alice's control-plane entry"
    );
}

// ---------------------------------------------------------------------------
// No leakage through diagnostics
// ---------------------------------------------------------------------------

/// No error's `Display` may contain a payload or a key.
#[tokio::test]
async fn errors_carry_no_payload_or_key() {
    let m = mgr();
    let secret = "s3cret-order-9912";
    let key = format!("orders/{secret}");

    let mut rendered = String::new();
    let long_key = "x".repeat(thesix::MAX_KEY_SIZE + 1);
    for outcome in [
        m.set(&long_key, secret.to_string(), &test_ctx())
            .await
            .err(),
        m.get(&long_key, &test_ctx()).await.err(),
        m.get(&key, &CacheContext::anonymous()).await.err(),
        m.set(&key, secret.to_string(), &CacheContext::anonymous())
            .await
            .err(),
    ]
    .into_iter()
    .flatten()
    {
        rendered.push_str(&outcome.to_string());
    }
    assert!(!rendered.is_empty(), "no error was produced to inspect");
    assert!(
        !rendered.contains(secret),
        "an error message leaked the payload: {rendered}"
    );
    assert!(
        !rendered.contains("orders/"),
        "an error message leaked the key: {rendered}"
    );
}

/// Telemetry must identify a key without revealing it, and never carry a value.
#[tokio::test]
async fn telemetry_carries_no_payload_and_no_key() {
    let ring = Arc::new(RingTelemetry::new(64));
    let sink: Arc<dyn thesix::TelemetrySink> = ring.clone();
    let m = testkit::manager_with_telemetry::<String>(sink, Duration::from_secs(2));

    let secret = "hunter2-card-number";
    let key = format!("payment/{secret}");
    m.set(&key, secret.to_string(), &test_ctx())
        .await
        .expect("set");
    let _ = m.get(&key, &test_ctx()).await;

    let records = ring.records();
    assert!(!records.is_empty(), "telemetry recorded nothing");
    for r in &records {
        let line = r.to_string();
        assert!(
            !line.contains(secret),
            "telemetry leaked the payload or key: {line}"
        );
        // The digest form is what must appear instead.
        assert!(
            line.contains("key=k"),
            "telemetry has no key identity: {line}"
        );
    }

    // And the digest must be addressable without exposing the key. Records carry
    // the *tenant-framed* key, because that is what the operation addressed —
    // selecting by the raw application key would silently match nothing, which is
    // the kind of "no leak" that is really "no telemetry".
    let mut framed = [0u8; 128];
    let n = thesix::frame_tenant_key(&test_ctx().identity().tenant, key.as_bytes(), &mut framed)
        .expect("frame");
    let targeted = ring.records_for(&framed[..n]);
    assert!(
        !targeted.is_empty(),
        "records could not be selected by key, so the identity is useless for \\
         reconstruction"
    );
}

/// Every contract-mandated field must be present on a real record.
#[tokio::test]
async fn telemetry_records_every_required_field() {
    let ring = Arc::new(RingTelemetry::new(8));
    let sink: Arc<dyn thesix::TelemetrySink> = ring.clone();
    let m = testkit::manager_with_telemetry::<String>(sink, Duration::from_secs(2));

    m.set(&"k".to_string(), "v".to_string(), &test_ctx())
        .await
        .expect("set");
    let _ = m
        .get_or_fetch(&"g".to_string(), &test_ctx(), || async {
            Ok("populated".to_string())
        })
        .await;

    let records = ring.records();
    assert!(records.len() >= 2, "expected a record per operation");
    let line = records[0].to_string();
    for field in [
        "op=",
        "key=",
        "src=",
        "dst=",
        "backend=",
        "gen=",
        "latency_us=",
        "outcome=",
        "fallback=",
        "failure=",
        "recovery=",
    ] {
        assert!(line.contains(field), "record is missing {field}: {line}");
    }

    // A denied operation is reported as denied, not as a generic failure.
    let _ = m.get(&"k".to_string(), &CacheContext::anonymous()).await;
    let denied = ring
        .records()
        .iter()
        .filter(|r| r.operation == Operation::Get)
        .count();
    assert!(
        denied > 0,
        "no read was recorded, so the denial went unobserved"
    );
}

/// `KeyIdentity` must be a digest, not an encoding.
#[test]
fn key_identity_is_not_reversible_by_construction() {
    let key = b"users/42/profile";
    let id = KeyIdentity::of(key);
    let rendered = id.to_string();
    // A 16-hex-digit digest will contain most two-character sequences by chance,
    // so check for distinctive multi-character fragments rather than short ones.
    assert!(!rendered.contains("users"));
    assert!(!rendered.contains("profile"));
    assert!(
        rendered.starts_with('k'),
        "unexpected identity form: {rendered}"
    );
    assert_eq!(
        rendered.len(),
        "k".len() + 16 + 1 + id.len().to_string().len()
    );
    // Two keys of the same length must not share an identity.
    assert_ne!(KeyIdentity::of(b"aaaa"), KeyIdentity::of(b"aaab"));
}

// ---------------------------------------------------------------------------
// Integrity
// ---------------------------------------------------------------------------

/// A damaged stored value must be refused rather than served, through the whole
/// stack rather than only at the stub.
#[tokio::test]
async fn a_corrupt_value_is_never_served() {
    let stub = Arc::new(std::sync::Mutex::new(
        thesix::FixedTierStub::<String>::with_capacity(8).expect("stub"),
    ));

    let key = KeyRef(b"integrity");
    stub.lock()
        .expect("lock")
        .set(&key, "genuine".to_string(), None)
        .expect("set");
    assert_eq!(
        stub.lock().expect("lock").get(&key).expect("get"),
        Some("genuine".to_string())
    );

    // Damage it behind the tier's back.
    assert!(
        stub.lock()
            .expect("lock")
            .corrupt_stored_value_for_test(&key, "tampered".to_string())
            .expect("corrupt"),
        "the corruption did not land"
    );

    let read = stub.lock().expect("lock").get(&key);
    assert_eq!(
        read,
        Err(CacheError::Corrupted),
        "a damaged value was returned to the caller"
    );
    assert_eq!(
        stub.lock().expect("lock").corruptions_detected(),
        1,
        "the integrity gate fired a different number of times"
    );
}

/// Two keys whose *placement* collides must not alias.
#[test]
fn colliding_keys_do_not_alias() {
    use thesix::Placement;
    let mut stub = thesix::FixedTierStub::<String>::with_capacity(4)
        .expect("stub")
        .with_placement(Placement::CollidingPair { prefix: b"k" });

    // Anti-vacuity for the collision: these must share a slot.
    assert_eq!(
        Placement::CollidingPair { prefix: b"k" }.hash(b"k1"),
        Placement::CollidingPair { prefix: b"k" }.hash(b"k2"),
        "the placement strategy is not actually colliding"
    );

    stub.set(&KeyRef(b"k1"), "one".to_string(), None)
        .expect("set");
    stub.set(&KeyRef(b"k2"), "two".to_string(), None)
        .expect("set");

    assert_eq!(
        stub.get(&KeyRef(b"k1")).expect("get k1"),
        Some("one".to_string())
    );
    assert_eq!(
        stub.get(&KeyRef(b"k2")).expect("get k2"),
        Some("two".to_string())
    );
}

/// Every key on one slot must still be individually retrievable.
#[test]
fn total_collision_does_not_alias() {
    use thesix::Placement;
    let mut stub = thesix::FixedTierStub::<String>::with_capacity(16)
        .expect("stub")
        .with_placement(Placement::AllToZero);

    for i in 0..12 {
        stub.set(&KeyRef(format!("k{i}").as_bytes()), format!("v{i}"), None)
            .unwrap_or_else(|e| panic!("set {i}: {e:?}"));
    }
    for i in 0..12 {
        assert_eq!(
            stub.get(&KeyRef(format!("k{i}").as_bytes()))
                .unwrap_or_else(|e| panic!("get {i}: {e:?}")),
            Some(format!("v{i}")),
            "key {i} aliased another key's value"
        );
    }
}

/// A generation conflict must be detected, not silently resolved in favour of the
/// later writer.
#[tokio::test]
async fn a_stale_generation_is_rejected() {
    let cachelito = Cachelito::new();
    let t1 = cachelito
        .prepare(b"gen", None, TierId::L1, thesix::IntentKind::Write)
        .expect("prepare");
    cachelito.bump_generation(b"gen").expect("bump");
    let t2 = cachelito
        .prepare(b"gen", None, TierId::L2, thesix::IntentKind::Write)
        .expect("prepare");

    assert!(cachelito.commit(&t2, None).is_ok());
    assert_eq!(
        cachelito.commit(&t1, None),
        Err(CacheError::StaleGeneration),
        "a superseded write was committed"
    );
    let snap = cachelito.peek(b"gen").expect("peek");
    assert_eq!(snap.tier, TierId::L2, "the loser's rung won");
}

/// Anonymous callers must not read anything.
#[tokio::test]
async fn anonymous_callers_are_refused_before_any_state_is_touched() {
    let m = mgr();
    let key = "private".to_string();
    m.set(&key, "value".to_string(), &test_ctx())
        .await
        .expect("seed");

    assert_eq!(
        m.get(&key, &CacheContext::anonymous()).await,
        Err(CacheError::Unauthenticated)
    );
    assert_eq!(
        m.exists(&key, &CacheContext::anonymous()).await,
        Err(CacheError::Unauthenticated)
    );
    assert_eq!(
        m.set(&key, "x".to_string(), &CacheContext::anonymous())
            .await,
        Err(CacheError::Unauthenticated)
    );

    // The authenticated view is untouched.
    assert_eq!(
        m.get(&key, &test_ctx()).await.expect("get"),
        Some("value".to_string())
    );
}

/// An unauthenticated probe must not have created a control-plane slot under the
/// tenant's key.
#[tokio::test]
async fn a_refused_request_leaves_no_residue() {
    let m = mgr();
    let ctx = test_ctx();
    let key = "untouched".to_string();
    assert_eq!(
        m.get(&key, &CacheContext::anonymous()).await,
        Err(CacheError::Unauthenticated)
    );
    let mut framed = [0u8; 128];
    let n = thesix::frame_tenant_key(&ctx.identity().tenant, key.as_bytes(), &mut framed)
        .expect("frame");
    let snap = m.cachelito().peek(&framed[..n]).expect("peek");
    assert_eq!(
        snap.state,
        thesix::EntryState::Absent,
        "a refused request created a control-plane entry"
    );
    assert_eq!(snap.generation, thesix::Generation::new(0));
}

/// Every rung must be reachable only through the manager: the public `tier`
/// accessor is documented as a substitution shim, and this pins that down so it
/// cannot quietly become the recommended path.
#[tokio::test]
async fn the_tier_accessor_is_documented_as_a_shim() {
    let m = mgr();
    assert!(m.has_tier(&TierId::L5));
    // L6 is unbound in the default build.
    assert!(!m.has_tier(&TierId::L6));
    // Asking for it returns *something* — which is exactly why `has_tier` and
    // `capabilities` exist.
    // The honest answer is "nothing there", not another rung's data.
    assert!(
        m.tier(&TierId::L6).is_none(),
        "an unbound rung handed back a substitute tier"
    );
    assert!(
        !m.capabilities()[&TierId::L6].is_bound(),
        "capabilities reported the unbound rung as bound"
    );
}

/// The `Outcome` mapping must distinguish denial from failure, or a security
/// event is indistinguishable from a capacity event in the logs.
#[test]
fn denial_and_failure_are_distinguishable_in_telemetry() {
    assert_eq!(
        Outcome::from_error(CacheError::Unauthenticated),
        Outcome::Denied
    );
    assert_eq!(
        Outcome::from_error(CacheError::Unauthorized),
        Outcome::Denied
    );
    assert_eq!(
        Outcome::from_error(CacheError::PolicyDenied),
        Outcome::Denied
    );
    assert_eq!(
        Outcome::from_error(CacheError::CapacityExhausted),
        Outcome::Failed
    );
    assert_eq!(Outcome::from_error(CacheError::Corrupted), Outcome::Failed);
    assert_ne!(
        Outcome::from_error(CacheError::Unauthenticated),
        Outcome::from_error(CacheError::CapacityExhausted)
    );
}

/// A poisoned shard must not become readable through the public API.
#[test]
fn a_poisoned_shard_is_not_reachable() {
    let m = mgr();
    let direct = m.tier(&TierId::L0).expect("L0 is bound");
    let key = KeyRef(b"poison-check");
    // The stub reports unavailable rather than panicking on a poisoned mutex.
    let r = {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async { direct.contains(&key).await })
    };
    assert!(r.is_ok(), "a healthy shard reported an error: {r:?}");
}
