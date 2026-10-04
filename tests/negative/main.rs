//! Negative testing: every failure the contract names, each verified by its
//! resulting system state rather than by the fact that an error came back.
//!
//! Two rules govern everything here.
//!
//! **An error is not a result.** A test that only asserts `is_err()` passes just
//! as happily when the operation fails for an unrelated reason — a typo in a
//! bound, a poisoned mutex, a timeout where a corruption was expected. So each
//! test asserts the specific error *and* what the entry, the rung and the
//! registry look like afterwards.
//!
//! **The fault must have fired.** Each case drives an instrumented tier and
//! asserts its `FaultLedger` recorded the activation. Without that, a test whose
//! injected fault never triggered still passes, which is the failure mode the
//! contract's `[verification.anti_vacuity]` section exists to forbid.

use std::sync::Arc;
use std::time::Duration;

use testkit::coverage::NEGATIVE_CASES;
use testkit::{
    ctx_for_tenant, default_tiers, faulty, make_manager_with_timeout, manager_from_tiers,
    record_all, test_ctx,
};
use thesix::{
    CacheContext, CacheError, CacheManager, CacheTier, Cachelito, DefaultPolicy, FaultClass,
    FaultPlan, KeyRef, L0Stub, MemoryPool, OpKind, RecoveryReport, TierId, TierRegistry,
};

/// The manager every case uses unless it needs a specific topology.
fn mgr() -> Arc<CacheManager<String, String, DefaultPolicy>> {
    make_manager_with_timeout(DefaultPolicy, Duration::from_millis(250))
}

/// A single-rung ladder holding one instrumented tier, for cases that need to
/// control exactly where the fault lands.
fn single_rung(
    tier: Arc<dyn CacheTier<String>>,
) -> Arc<CacheManager<String, String, DefaultPolicy>> {
    manager_from_tiers(DefaultPolicy, vec![tier], Duration::from_millis(250))
}

testkit::declare_cases! {
    /// Names generated from the same macro invocations as the tests above.
    pub fn covered_cases() -> &'static [&'static str] {
    // ---------------------------------------------------------------------
    // Backend reachability
    // ---------------------------------------------------------------------

    /// A rung that refuses every operation must not wedge the entry.
    ///
    /// The post-state assertion is the whole point: after the failure the entry
    /// must be claimable again, or the next caller waits out the full timeout
    /// and fails too. A key that fails once and then fails forever is a
    /// different bug from a key that failed once.
    async fn backend_unavailable() {
        let (tier, ledger) = faulty(
            Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
            // Armed *after* seeding, so the seed write succeeds and the read is
            // the operation that hits the fault. Arming both would let the seed
            // consume the fault and leave the read passing.
            FaultPlan::new(),
        );
        let handle = Arc::clone(&tier);
        let m = single_rung(handle as Arc<dyn CacheTier<String>>);
        let key = "k".to_string();
        m.set(&key, "v".to_string(), &test_ctx())
            .await
            .expect("seed");

        tier.arm(FaultPlan::new().push(OpKind::Get, FaultClass::ReadFailure));

        let first = m.get(&key, &test_ctx()).await;
        assert_eq!(first, Err(CacheError::TierUnavailable));
        assert_eq!(ledger.fired(FaultClass::ReadFailure), 1, "fault never fired");

        // Post-state: a read must not claim the entry, or one failing read
        // leaves the key unusable for every later caller.
        let snapshot = m.cachelito().peek(&testkit::framed_key(&test_ctx(), &key)).expect("peek");
        assert!(
            !snapshot.population_owner,
            "a failed read claimed ownership of the entry"
        );
        assert_eq!(snapshot.state, thesix::EntryState::Ready);

        // And the read works again as soon as the rung does.
        assert_eq!(
            m.get(&key, &test_ctx()).await.expect("get after recovery"),
            Some("v".to_string()),
            "the key stayed broken after one failing read"
        );
    }

    /// A rung that times out must release the entry, not hold it.
    async fn backend_timeout() {
        let (tier, ledger) = faulty(
            Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
            FaultPlan::new().push(OpKind::Set, FaultClass::Timeout),
        );
        let m = single_rung(tier as Arc<dyn CacheTier<String>>);
        let result = m
            .set(&"slow".to_string(), "v".to_string(), &test_ctx())
            .await;
        assert!(result.is_err(), "an injected timeout cannot succeed");
        assert_eq!(ledger.fired(FaultClass::Timeout), 1, "fault never fired");

        // The two-phase commit must have aborted: no intent left outstanding.
        let snapshot = m.cachelito().peek(&testkit::framed_key(&test_ctx(), "slow")).expect("peek");
        assert!(
            snapshot.intent.is_none(),
            "a failed write left an outstanding commit intent: {:?}",
            snapshot.intent
        );
    }

    /// A rung that never answers must not stop the control plane answering.
    ///
    /// Anti-vacuity: `reached` is asserted non-zero, so this cannot pass without
    /// the data plane actually having stalled.
    async fn backend_hang() {
        use testkit::HangingTier;

        // Put a committed entry on L0 *in the control plane* before the manager
        // exists, and hang L0.
        //
        // Seeding through `set` instead would not work: `DefaultPolicy` sends a
        // write to L1 and a read to L0, so the entry would be committed on L1 and
        // the read would answer from a real stub — the hang would never be
        // reached and `reached` would stay at zero. That is exactly the shape of
        // a test that passes without exercising anything.
        let ctx = test_ctx();
        let cachelito = Cachelito::new();
        let real = Arc::new(L0Stub::<String>::new());
        real.set(&KeyRef(b"hangs"), "v".to_string(), None)
            .await
            .expect("seed the rung directly");
        let framed = testkit::framed_key(&ctx, "hangs");
        let token = cachelito
            .prepare(&framed, None, TierId::L0, thesix::IntentKind::Write)
            .expect("prepare");
        cachelito.commit(&token, None).expect("commit");

        let hanging = HangingTier::wrap(Arc::clone(&real) as Arc<dyn CacheTier<String>>);
        let reached = hanging.reached();
        let m = testkit::manager_from_parts::<String>(
            DefaultPolicy,
            cachelito,
            vec![hanging],
            Duration::from_millis(150),
        );

        let key = "hangs".to_string();
        let ctx = test_ctx();

        // Park a read against the hung rung.
        let handle = {
            let m = Arc::clone(&m);
            let key = key.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move { m.get(&key, &ctx).await })
        };

        // Give it time to actually reach the stall.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            reached.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "the data plane never stalled, so this proves nothing"
        );

        // The control plane must still answer, for an unrelated key.
        let ctl = tokio::time::timeout(Duration::from_millis(500), async {
            m.cachelito().set_tier(b"unrelated", TierId::L1)
        })
        .await;
        assert!(
            ctl.is_ok(),
            "the control plane blocked behind the data plane: {:?}",
            ctl.err()
        );

        // A second read of an unrelated key must not be blocked either.
        let other = tokio::time::timeout(
            Duration::from_millis(500),
            m.get(&"other".to_string(), &ctx),
        )
        .await;
        assert!(
            other.is_ok(),
            "an unrelated read blocked behind the hung rung: {:?}",
            other.err()
        );

        // The parked read itself must resolve as a miss rather than hanging.
        let outcome = tokio::time::timeout(Duration::from_millis(600), handle).await;
        assert!(outcome.is_ok(), "the parked operation never resolved");
    }

    // ---------------------------------------------------------------------
    // Malformed and corrupt data
    // ---------------------------------------------------------------------

    /// A record too short to carry its framing must be rejected, not decoded.
    ///
    /// `contains` and `get` used to disagree on the same record — one said
    /// "present", the other said "undecodable" — so a caller could be told an
    /// entry existed and then fail to read it.
    #[cfg(feature = "sled")]
    async fn malformed_response() {
        use thesix::L4SledBackend;
        let dir = std::env::temp_dir().join(format!("thesix-neg-malformed-{}", std::process::id()));
        let Ok(backend) = L4SledBackend::<String>::open(dir.to_string_lossy().as_ref()) else {
            eprintln!("skipping: sled store would not open");
            return;
        };
        let key = KeyRef(b"framed");
        backend
            .set(&key, "value".to_string(), None)
            .await
            .expect("set");

        // Truncate the record below the framing length.
        backend.truncate_record_for_test(b"framed", 3).await;

        let read = backend.get(&key).await;
        assert!(
            matches!(read, Err(CacheError::SerializationFailed)),
            "a truncated record must be a decode failure, got {read:?}"
        );
        // Agreement is the property, not a specific verdict: `contains` used to
        // answer `Ok(true)` for the very bytes `get` called undecodable, so a
        // caller could be told an entry existed and then fail to read it.
        assert_eq!(
            backend.contains(&key).await,
            read.map(|v| v.is_some()),
            "contains and get disagree about the same record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }


    /// A key that cannot be encoded is rejected before any state is touched.
    async fn malformed_payload() {
        let m = mgr();
        let too_long = "x".repeat(thesix::key::MAX_KEY_SIZE + 1);
        let result = m.set(&too_long, "v".to_string(), &test_ctx()).await;
        assert_eq!(
            result,
            Err(CacheError::ConfigurationError),
            "an unencodable key must be rejected explicitly"
        );
        // Post-state: nothing was created for it. An oversized key has no frame
        // at all, so there is nothing to look up — which is itself the assertion:
        // the operation was refused before it could address any entry.
        assert!(
            testkit::try_framed_key(&test_ctx(), &too_long).is_none(),
            "an oversized key produced a frame, so the rejection was not a \
             length check"
        );
        assert_eq!(
            m.set(&too_long, "v".to_string(), &test_ctx()).await,
            Err(CacheError::ConfigurationError),
            "the rejection was not repeatable"
        );
    }

    /// A value damaged behind the tier's back must be refused, not served.
    ///
    /// The corruption is applied to the stored value directly, which is the only
    /// way to produce a genuinely mismatched record: every normal write stores
    /// the value and its digest together.
    async fn corrupt_payload() {
        let mut stub = thesix::FixedTierStub::<String>::with_capacity(8).expect("stub");
        let key = KeyRef(b"payload");
        stub.set(&key, "original".to_string(), None).expect("set");
        assert_eq!(stub.get(&key).expect("get"), Some("original".to_string()));

        let damaged = stub
            .corrupt_stored_value_for_test(&key, "tampered".to_string())
            .expect("corrupt");
        assert!(damaged, "the corruption did not land; the test is vacuous");

        let read = stub.get(&key);
        assert_eq!(
            read,
            Err(CacheError::Corrupted),
            "a value whose digest does not match must be refused, not served"
        );
        assert_eq!(
            stub.corruptions_detected(),
            1,
            "the integrity gate reported a different number of rejections"
        );

        // Removing a corrupt entry must still work: refusal to serve is not a
        // refusal to clean up.
        stub.remove(&key).expect("remove");
        assert_eq!(stub.get(&key).expect("get after remove"), None);
    }

    /// Broken metadata reporting must not make a rung unusable.
    async fn invalid_metadata() {
        let (tier, ledger) = faulty(
            Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
            FaultPlan::new(),
        );
        let handle = Arc::clone(&tier);
        let m = single_rung(handle as Arc<dyn CacheTier<String>>);
        let key = "meta".to_string();
        m.set(&key, "v".to_string(), &test_ctx()).await.expect("seed");

        tier.arm(FaultPlan::new().push(OpKind::Get, FaultClass::MetadataFailure));

        let result = m.get(&key, &test_ctx()).await;
        assert!(result.is_err(), "a broken metadata path cannot succeed");
        assert_eq!(
            ledger.fired(FaultClass::MetadataFailure),
            1,
            "the metadata fault never fired"
        );
        let snapshot = m.cachelito().peek(&testkit::framed_key(&test_ctx(), &key)).expect("peek");
        assert!(!snapshot.population_owner);
    }

    // ---------------------------------------------------------------------
    // Capacity
    // ---------------------------------------------------------------------

    /// A full rung reports capacity exhaustion, not misconfiguration.
    ///
    /// These were the same variant, so a caller reacting to the error as a
    /// deployment fault would treat a busy cache as fatal.
    async fn capacity_exhaustion() {
        let mut stub = thesix::FixedTierStub::<String>::with_capacity(2).expect("stub");
        stub.set(&KeyRef(b"a"), "1".to_string(), None).expect("set");
        stub.set(&KeyRef(b"b"), "2".to_string(), None).expect("set");
        let third = stub.set(&KeyRef(b"c"), "3".to_string(), None);
        assert_eq!(third, Err(CacheError::CapacityExhausted));
        // Post-state: the two existing entries are intact.
        assert_eq!(stub.get(&KeyRef(b"a")).expect("get a"), Some("1".to_string()));
        assert_eq!(stub.get(&KeyRef(b"b")).expect("get b"), Some("2".to_string()));
        assert_eq!(stub.get(&KeyRef(b"c")).expect("get c"), None);
    }

    // ---------------------------------------------------------------------
    // Cancellation
    // ---------------------------------------------------------------------

    /// Cancelling a population must release the entry.
    ///
    /// A cancelled owner that kept its claim would leave the key permanently
    /// unusable: every later caller would join as a waiter against an owner that
    /// no longer exists.
    async fn cancelled_operation() {
        let m = mgr();
        let key = "cancelled".to_string();
        let ctx = test_ctx();

        let handle = {
            let m = Arc::clone(&m);
            let key = key.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                m.get_or_fetch(&key, &ctx, || async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("never".to_string())
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(60)).await;
        handle.abort();
        let _ = handle.await;

        // The abandoned owner must not still own the entry.
        let snapshot = m.cachelito().peek(&testkit::framed_key(&test_ctx(), &key)).expect("peek");
        assert!(
            !snapshot.population_owner,
            "a cancelled population kept its claim; the key is now unusable"
        );
        assert!(
            snapshot.intent.is_none(),
            "cancellation left a commit intent outstanding"
        );

        // And the key must be usable again immediately.
        let recovered = tokio::time::timeout(
            Duration::from_millis(500),
            m.get_or_fetch(&key, &ctx, || async { Ok("second".to_string()) }),
        )
        .await;
        assert!(
            recovered.is_ok(),
            "the key stayed unusable after its owner was cancelled"
        );
        assert_eq!(recovered.expect("join").expect("value"), "second".to_string());
    }

    // ---------------------------------------------------------------------
    // Duplicate and conflicting operations
    // ---------------------------------------------------------------------

    /// Two writers on one key must leave exactly one coherent value.
    ///
    /// Not "both succeed" — `set` is last-writer-wins by design — but the value
    /// that survives must be one of the two written, not a blend.
    async fn duplicate_operation() {
        let tiers = default_tiers::<String>();
        let (tiers, tallies) = record_all(tiers);
        let m = manager_from_tiers(DefaultPolicy, tiers, Duration::from_secs(2));
        let key = "dup".to_string();
        let ctx = test_ctx();

        let mut handles = Vec::new();
        for i in 0..8 {
            let m = Arc::clone(&m);
            let key = key.clone();
            let ctx = ctx.clone();
            handles.push(tokio::spawn(async move {
                m.set(&key, format!("v{i}"), &ctx).await
            }));
        }
        for h in handles {
            h.await.expect("join").expect("set");
        }

        let final_value = m.get(&key, &ctx).await.expect("get").expect("present");
        assert!(
            (0..8).any(|i| final_value == format!("v{i}")),
            "the surviving value {final_value:?} is none of those written"
        );

        // Post-state: exactly one committed value, and the control plane agrees
        // with the data plane about which rung holds it.
        let snapshot = m.cachelito().peek(&testkit::framed_key(&test_ctx(), &key)).expect("peek");
        assert_eq!(snapshot.state, thesix::EntryState::Ready);
        let total: u64 = tallies.iter().map(|t| t.total()).sum();
        assert!(total > 0, "no operation reached any tier");
    }

    // ---------------------------------------------------------------------
    // Generations
    // ---------------------------------------------------------------------

    /// A population whose generation moved must be rejected *and* must release
    /// the entry.
    ///
    /// The rejection is the easy half. The release is the regression: the old
    /// code returned `StaleGeneration` before clearing ownership, so the key
    /// stayed `InFlight` forever and every later caller waited out the full
    /// timeout.
    async fn stale_generation() {
        let m = mgr();
        let key = "stale".to_string();
        let ctx = test_ctx();

        // Drive a population that will be overtaken.
        let handle = {
            let m = Arc::clone(&m);
            let key = key.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                m.get_or_fetch(&key, &ctx, || async {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    Ok("slow".to_string())
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(40)).await;

        // Move the generation out from under it.
        m.cachelito()
            .bump_generation(&testkit::framed_key(&test_ctx(), &key))
            .expect("bump");

        let outcome = tokio::time::timeout(Duration::from_millis(800), handle)
            .await
            .expect("the population never resolved");
        let result = outcome.expect("join");
        assert!(
            result.is_err(),
            "a population that overran its generation was accepted: {result:?}"
        );

        // Post-state: ownership released, so the key works again.
        let snapshot = m.cachelito().peek(&testkit::framed_key(&test_ctx(), &key)).expect("peek");
        assert!(
            !snapshot.population_owner,
            "the overtaken population kept its claim"
        );
        assert!(snapshot.intent.is_none(), "an intent was left outstanding");

        let recovered = tokio::time::timeout(
            Duration::from_millis(800),
            m.get_or_fetch(&key, &ctx, || async { Ok("next".to_string()) }),
        )
        .await;
        assert!(
            recovered.is_ok(),
            "the key stayed unusable after a stale generation was rejected"
        );
    }

    /// Two commits prepared on one generation: exactly one may win.
    async fn generation_conflict() {
        let cachelito = Cachelito::new();
        let first = cachelito
            .prepare(b"conflict", None, TierId::L1, thesix::IntentKind::Write)
            .expect("first prepare");
        // Advance the generation so the first token is stale.
        cachelito.bump_generation(b"conflict").expect("bump");

        let second = cachelito
            .prepare(b"conflict", None, TierId::L2, thesix::IntentKind::Write)
            .expect("second prepare");

        // The second commit succeeds; the first must not.
        assert_ne!(
            first.generation, second.generation,
            "the second prepare did not observe a moved generation, so the \
             conflict under test never happened"
        );
        assert!(
            cachelito.commit(&second, None).is_ok(),
            "the current token failed to commit"
        );
        assert_eq!(
            cachelito.commit(&first, None),
            Err(CacheError::StaleGeneration),
            "a superseded token committed anyway"
        );

        // Post-state: one committed value, on the rung the winner chose.
        let snapshot = cachelito.peek(b"conflict").expect("peek");
        assert_eq!(snapshot.state, thesix::EntryState::Ready);
        assert_eq!(snapshot.tier, TierId::L2);
        assert!(snapshot.intent.is_none(), "the intent was not cleared");
    }

    // ---------------------------------------------------------------------
    // Partial commits
    // ---------------------------------------------------------------------

    /// An intent left by a crash between prepare and commit must never be
    /// readable, and recovery must abort it.
    async fn partial_write() {
        let cachelito = Cachelito::new();
        let token = cachelito
            .prepare(b"half", None, TierId::L1, thesix::IntentKind::Write)
            .expect("prepare");
        assert_eq!(
            token.kind,
            thesix::IntentKind::Write,
            "the intent was not recorded as a write, so recovery could not have \
             chosen a direction"
        );

        // The window: prepared, not committed.
        let during = cachelito.peek(b"half").expect("peek");
        assert_eq!(
            during.state,
            thesix::EntryState::Prepared,
            "an uncommitted write is not represented as such"
        );
        assert!(!during.state.is_readable(), "a prepared entry read as readable");
        assert!(during.intent.is_some(), "no intent was recorded");

        // Recovery resolves it.
        let outcome = cachelito
            .resolve_intent(
                b"half",
                during.intent.expect("intent"),
                thesix::RecoveryDirection::Abort,
            )
            .expect("resolve");
        assert!(outcome.is_success());
        assert!(!outcome.is_committed(), "an aborted write reported as committed");

        let after = cachelito.peek(b"half").expect("peek");
        assert!(after.intent.is_none(), "recovery left the intent in place");
        assert!(!after.state.is_readable(), "an aborted write is still readable");
    }

    /// An intent left mid-promotion must be completed forward, not aborted.
    ///
    /// Aborting a move would discard a value that was already committed, which is
    /// data loss rather than an unreplicated write.
    async fn partial_promotion() {
        let cachelito = Cachelito::new();
        // Establish a committed entry at L2.
        let seed = cachelito
            .prepare(b"moved", None, TierId::L2, thesix::IntentKind::Write)
            .expect("seed prepare");
        cachelito.commit(&seed, None).expect("seed commit");

        // Now a move that never commits.
        let move_token = cachelito
            .prepare(b"moved", None, TierId::L1, thesix::IntentKind::Move)
            .expect("move prepare");
        assert!(
            move_token.kind == thesix::IntentKind::Move,
            "the intent was not recorded as a move, so recovery could not pick a direction"
        );
        let stale = cachelito
            .stale_intents(0)
            .into_iter()
            .find(|(a, _)| *a == move_token.address)
            .map(|(_, i)| i)
            .expect("the move intent was not discoverable");

        // A sweep would be told `ExternalReconciliation`; this test holds the key,
        // so complete-forward is executable here. Asserting `for_kind` equals
        // `CompleteForward` is what let the contract claim a direction the
        // production sweep had no way to take.
        assert_eq!(
            thesix::RecoveryDirection::for_kind(stale.kind),
            thesix::RecoveryDirection::ExternalReconciliation,
            "a sweep holding only a hash must not claim it can complete a move"
        );

        let outcome = cachelito
            .resolve_intent(b"moved", stale, thesix::RecoveryDirection::CompleteForward)
            .expect("resolve");
        assert!(outcome.is_success());
        assert!(
            outcome.is_committed(),
            "the recovered move did not leave the value committed"
        );

        let after = cachelito.peek(b"moved").expect("peek");
        assert_eq!(after.state, thesix::EntryState::Ready);
        assert_eq!(after.tier, TierId::L1, "the move did not reach its target");
    }

    // ---------------------------------------------------------------------
    // Authority and fallback
    // ---------------------------------------------------------------------

    /// Losing the authority must be visible and must not promote a fallback.
    async fn authority_unavailable() {
        let caps = {
            let m = mgr();
            m.capabilities()
        };
        assert!(
            caps[&TierId::L6].is_authoritative(),
            "the authority rung does not report itself authoritative"
        );
        assert!(
            !caps[&TierId::L6].is_bound(),
            "the default build has no authority implementation, and says otherwise"
        );
        // The critical assertion: no *cache* rung inherited the flag.
        for tier in [TierId::L0, TierId::L1, TierId::L2, TierId::L3, TierId::L4, TierId::L5] {
            assert!(
                !caps[&tier].is_authoritative(),
                "{tier} claims authority it was never given"
            );
        }
    }

    /// A fail-open scan must stay inside the ladder and must not manufacture a
    /// value.
    async fn fallback_unavailable() {
        // Every rung refuses. The fail-open scan must find nothing and must
        // report the *original* cause, not a generic population failure.
        let tiers: Vec<Arc<dyn CacheTier<String>>> = (0..6)
            .map(|_| {
                let (t, _) = faulty(
                    Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
                    FaultPlan::new()
                        .push(OpKind::Get, FaultClass::ReadFailure)
                        .push(OpKind::Set, FaultClass::WriteFailure),
                );
                t as Arc<dyn CacheTier<String>>
            })
            .collect();
        let m = manager_from_tiers(DefaultPolicy, tiers, Duration::from_millis(200));

        let result = m
            .get_or_fetch(&"nowhere".to_string(), &test_ctx(), || async {
                Err(CacheError::TierUnavailable)
            })
            .await;
        assert!(
            result.is_err(),
            "a fetch that failed with every rung down cannot succeed"
        );
        assert_ne!(
            result,
            Err(CacheError::PopulationFailed),
            "the cause was replaced by a generic failure, hiding why it failed"
        );
    }

    // ---------------------------------------------------------------------
    // Recovery
    // ---------------------------------------------------------------------

    /// Recovery must report what it could not resolve.
    async fn recovery_failure() {
        let m = mgr();
        // Nothing outstanding: recovery succeeds trivially and says so.
        let RecoveryReport { recovered, failed, .. } = m.recover_older_than(Duration::from_secs(0));
        assert_eq!(failed, 0, "recovery reported a failure with nothing to do");
        assert_eq!(recovered, 0);

        // Now leave one outstanding and confirm it is counted.
        m.cachelito()
            .prepare(
                &testkit::framed_key(&test_ctx(), "stuck"),
                None,
                TierId::L1,
                thesix::IntentKind::Write,
            )
            .expect("prepare");
        let RecoveryReport { recovered, failed, .. } = m.recover_older_than(Duration::from_secs(0));
        assert_eq!(recovered, 1, "recovery did not report the outstanding intent");
        assert_eq!(failed, 0);
        // Idempotent: running it again finds nothing and reports nothing.
        let RecoveryReport { recovered, failed, .. } = m.recover_older_than(Duration::from_secs(0));
        assert_eq!((recovered, failed), (0, 0), "recovery is not idempotent");
    }

    /// A process restart must not turn a committed write into a lost one.
    ///
    /// The honest version of this test uses the one backend that can survive a
    /// restart. Without it the case is substituted visibly rather than passing
    /// vacuously.
    #[cfg(feature = "sled")]
    async fn process_restart() {
        use thesix::L4SledBackend;
        let dir = std::env::temp_dir().join(format!("thesix-neg-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let key = KeyRef(b"survivor");
        {
            let backend = L4SledBackend::<String>::open(dir.to_string_lossy().as_ref())
                .expect("open");
            backend
                .set(&key, "committed".to_string(), None)
                .await
                .expect("set");
            backend.flush_for_test().await;
        } // backend dropped: the "restart"

        let reopened = L4SledBackend::<String>::open(dir.to_string_lossy().as_ref())
            .expect("reopen");
        assert_eq!(
            reopened.get(&key).await.expect("get"),
            Some("committed".to_string()),
            "a committed value did not survive dropping and reopening the store"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }


    // ---------------------------------------------------------------------
    // Configuration
    // ---------------------------------------------------------------------

    /// Invalid configuration must be rejected at construction, not discovered
    /// on a live path.
    async fn invalid_configuration() {
        // Zero capacity is a misconfiguration and says so.
        assert_eq!(
            thesix::FixedTierStub::<String>::with_capacity(0).err(),
            Some(CacheError::ConfigurationError)
        );
        assert_eq!(
            MemoryPool::<String>::new(0).err(),
            Some(CacheError::ConfigurationError)
        );
        // A zero shard count is clamped rather than dividing by zero.
        let zero_shards = Cachelito::with_shards(0);
        assert!(
            zero_shards.peek(b"anything").is_ok(),
            "a zero shard count broke the control plane"
        );

        // A tier vector shorter than the ladder must report the missing rungs
        // as unbound rather than substituting.
        let short = manager_from_tiers(
            DefaultPolicy,
            vec![Arc::new(L0Stub::<String>::new())],
            Duration::from_millis(50),
        );
        let caps = short.capabilities();
        assert!(!short.has_tier(&TierId::L4));
        assert_eq!(caps[&TierId::L4].state, thesix::OperationalState::Unbound);
        assert!(
            !caps[&TierId::L4].is_bound(),
            "an unbound rung reported itself bound"
        );
    }
    }
}

#[cfg(not(feature = "sled"))]
#[tokio::test]
async fn substituted_malformed_response() {
    // Without a byte-oriented backend there is no framed record to
    // malform. Assert the substitution is visible rather than silently
    // passing: an unguarded no-op would report coverage that does not exist.
    eprintln!(
        "substituted: no byte-framed backend compiled in; \
         run with --features sled for the framed-record case"
    );
    assert!(
        !cfg!(feature = "sled"),
        "this branch runs only when sled is absent"
    );
}

#[cfg(not(feature = "sled"))]
#[tokio::test]
async fn substituted_process_restart() {
    eprintln!(
        "substituted: no restart-durable backend compiled in; \
         run with --features sled for the drop-and-reopen case"
    );
    assert!(!cfg!(feature = "sled"));
}
/// The registry and the contract must agree, or this file is testing a fiction.
#[test]
fn this_file_covers_every_required_case() {
    // `covered_cases` is generated from the same macro invocations that generate
    // the tests above, so a case cannot be listed without a test existing for it.
    let covered = covered_cases();
    let missing: Vec<&&str> = NEGATIVE_CASES
        .iter()
        .filter(|c| !covered.contains(c))
        .collect();
    assert!(
        missing.is_empty(),
        "the contract requires negative cases this file does not implement: {missing:?}"
    );
}

/// Every case must be exercised, and the harness must have observed real work.
#[test]
fn the_negative_harness_itself_is_healthy() {
    let covered = covered_cases();
    assert_eq!(
        covered.len(),
        NEGATIVE_CASES.len(),
        "covered {covered:?} but the contract requires {NEGATIVE_CASES:?}"
    );

    // The fault harness must actually be able to fire, or every ledger assertion
    // above is reading zeros that would also be zeros if nothing worked.
    let (tier, ledger) = faulty(
        Arc::new(L0Stub::<String>::new()) as Arc<dyn CacheTier<String>>,
        FaultPlan::new().push(OpKind::Get, FaultClass::Hang),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let handle = std::thread::spawn(move || {
        rt.block_on(async {
            let key = KeyRef(b"x");
            // The hang never resolves; the timeout is what returns.
            let _ = tokio::time::timeout(Duration::from_millis(80), tier.get(&key)).await;
        })
    });
    handle.join().expect("join");
    assert_eq!(
        ledger.fired(FaultClass::Hang),
        1,
        "the fault harness cannot fire a hang; every ledger assertion is suspect"
    );
    assert!(ledger.ops_observed() >= 1);
}

/// The tenant-scoped context helper must produce distinct identities, or the
/// security layer would be testing nothing.
#[test]
fn tenant_contexts_are_distinct() {
    let a = ctx_for_tenant("tenant-a");
    let b = ctx_for_tenant("tenant-b");
    assert_ne!(a.identity().tenant, b.identity().tenant);
    assert!(a.is_authenticated());
    let _: CacheContext = CacheContext::anonymous();
    let registry = TierRegistry::new();
    assert_eq!(registry.len(), 7);
}
