//! The autonomy layer: what the governor does, and what it refuses to do.
//!
//! Every case here is bound to a clause in `[autonomy]` via `testkit::proves!`,
//! and the anti-vacuity rules apply to autonomy exactly as they do to the data
//! plane: a test that claims the governor moved an entry asserts the entry's
//! control-plane rung changed, and a test that claims the manager path was used
//! asserts that manager's own telemetry recorded the move.

use std::sync::Arc;
use std::time::Duration;

use testkit::{ctx_for_tenant, framed_key};
use thesix::{
    CacheContext, CacheManager, Cachelito, DefaultPolicy, EntryState, Governor, GovernorMode,
    GovernorSettings, IntentKind, KeyCensus, KeyRef, MemoryPool, Operation, OperationRecord,
    Outcome, RingTelemetry, TelemetrySink, TierId, TierRegistry,
};

type M = CacheManager<String, String, DefaultPolicy>;

fn governor_manager() -> Arc<M> {
    let pool = MemoryPool::new(1024).expect("pool");
    Arc::new(CacheManager::with_timeout(
        DefaultPolicy,
        Cachelito::new(),
        TierRegistry::new(),
        testkit::default_tiers(),
        pool,
        Duration::from_secs(5),
    ))
}

fn settings(mode: GovernorMode) -> GovernorSettings {
    GovernorSettings {
        mode,
        hot_heat: 2,
        max_moves_per_tick: 8,
        ..GovernorSettings::default()
    }
}

/// Seed one committed entry on L1 by the two-phase path, so its state is exactly
/// what a real write leaves behind and the governor's observation of it is not
/// an accident of a shortcut.
async fn seed_on_l1(m: &M, ctx: &CacheContext, key: &str, value: &str) {
    let framed = framed_key(ctx, key);
    let tier = m.bound_tier(&TierId::L1).expect("default ladder binds L1");
    let token = m
        .cachelito()
        .prepare(&framed, None, TierId::L1, IntentKind::Write)
        .expect("prepare the seed");
    tier.set(&KeyRef(&framed), value.to_string(), None)
        .await
        .expect("stub stores the seed");
    m.cachelito().commit(&token, None).expect("commit the seed");
}

/// Feed the heat ring directly: `RingTelemetry` is injected, so a test may
/// populate it the way the manager would. The record's key is the framed key,
/// which is exactly the identity the census derives.
fn feed_heat(ring: &RingTelemetry, framed: &[u8], reads: u64) {
    for _ in 0..reads {
        ring.record(
            &OperationRecord::begin(Operation::GetOrFetch, &KeyRef(framed))
                .finish(std::time::Instant::now(), Outcome::Served),
        );
    }
}

/// A manager, a census over `registered`, and a heat ring fed with `hot`.
async fn setup(
    registered: &[&str],
    hot: &[(&str, u64)],
) -> (
    Arc<M>,
    Arc<KeyCensus<String>>,
    Arc<RingTelemetry>,
    CacheContext,
) {
    // The ring is attached to the manager as its telemetry sink *and* handed to
    // the governor as its heat source -- the one ring, shared, which is the
    // wiring the clause about "runs through the manager" is proved against.
    let ring = Arc::new(RingTelemetry::new(4096));
    let owned_ring: Arc<RingTelemetry> = Arc::clone(&ring);
    let sink: thesix::SharedSink = owned_ring;
    let pool = MemoryPool::new(1024).expect("pool");
    let m = Arc::new(
        CacheManager::with_timeout(
            DefaultPolicy,
            Cachelito::new(),
            TierRegistry::new(),
            testkit::default_tiers(),
            pool,
            Duration::from_secs(5),
        )
        .with_shared_telemetry(sink),
    );
    let ctx = ctx_for_tenant("test-tenant");
    for (key, _) in hot {
        seed_on_l1(&m, &ctx, key, "seeded").await;
    }
    let census = Arc::new(KeyCensus::new(ctx_for_tenant("test-tenant")));
    for key in registered {
        census
            .register(key.to_string())
            .expect("test keys fit the frame");
    }
    for (key, reads) in hot {
        feed_heat(&ring, &framed_key(&ctx, key), *reads);
    }
    (m, census, ring, ctx)
}

fn governor(
    m: Arc<M>,
    mode: GovernorMode,
    census: Arc<KeyCensus<String>>,
    ring: Arc<RingTelemetry>,
    recover_after: Option<Duration>,
) -> Governor<String, String, DefaultPolicy> {
    let mut s = settings(mode);
    s.recover_after = recover_after;
    Governor::new(m, s).with_census(census).with_heat(ring)
}

#[test]
fn the_modes_escalate_by_execution_surface() {
    testkit::proves!(
        "autonomy.mode_is_escalating",
        "autonomy.manual_mode_executes_nothing",
        "autonomy.semi_mode_executes_maintenance_only"
    );

    assert!(
        !GovernorMode::Manual.executes_maintenance() && !GovernorMode::Manual.executes_moves(),
        "manual observes only"
    );
    assert!(
        GovernorMode::SemiAutonomous.executes_maintenance()
            && !GovernorMode::SemiAutonomous.executes_moves(),
        "semi maintains the control plane but never moves entries"
    );
    assert!(
        GovernorMode::FullyAutonomous.executes_maintenance()
            && GovernorMode::FullyAutonomous.executes_moves(),
        "fully does everything bounded"
    );

    // The escalation also gates spawning: a loop that would do nothing is a
    // configuration mistake, not a task.
    for mode in [GovernorMode::Manual, GovernorMode::SemiAutonomous] {
        let governor = Governor::new(governor_manager(), settings(mode));
        assert!(
            Arc::new(governor).spawn().is_err(),
            "{mode:?} must refuse to spawn a loop"
        );
    }
}

#[tokio::test]
async fn manual_tick_returns_a_plan_and_executes_nothing() {
    testkit::proves!("autonomy.manual_mode_executes_nothing");

    let (m, census, ring, _ctx) = setup(&["hot-1"], &[("hot-1", 5)]).await;
    let governor = governor(m.clone(), GovernorMode::Manual, census, ring, None);

    let report = governor.tick().await.expect("tick");
    assert_eq!(report.recovered_intents, 0);
    assert_eq!(report.expired_demoted, 0);
    assert_eq!(report.promoted, 0);
    assert_eq!(report.demoted, 0);
    assert!(report.plan.recovery_sweep && report.plan.expire_sweep);
    assert_eq!(
        report.plan.promote,
        vec![0],
        "the plan still says what should happen"
    );

    // And nothing changed: the entry sits on L1 exactly as seeded.
    let framed = framed_key(&ctx_for_tenant("test-tenant"), "hot-1");
    assert_eq!(m.cachelito().peek(&framed).expect("peek").tier, TierId::L1);
}

#[tokio::test]
async fn semi_tick_executes_maintenance_and_leaves_movement_unapplied() {
    testkit::proves!("autonomy.semi_mode_executes_maintenance_only");

    let (m, census, ring, ctx) = setup(&["hot-1"], &[("hot-1", 5)]).await;
    let governor = governor(
        m.clone(),
        GovernorMode::SemiAutonomous,
        census,
        ring,
        Some(Duration::ZERO),
    );

    // An outstanding intent left by some half-finished operation is what the
    // recovery leg exists for; semi executes that sweep. The intent is on its
    // own key, so the seeded entry stays `Ready` and plan.promote still names it.
    let framed = framed_key(&ctx, "abandoned-op");
    let _ = m
        .cachelito()
        .prepare(&framed, None, TierId::L1, IntentKind::Write)
        .expect("craft an intent");

    let report = governor.tick().await.expect("tick");
    assert_eq!(report.recovered_intents, 1, "semi runs the recovery sweep");
    assert_eq!(
        report.plan.promote,
        vec![0],
        "the plan still suggests the move"
    );
    assert_eq!(
        report.promoted + report.demoted,
        0,
        "semi plans movement but must not apply it"
    );
}

#[tokio::test]
async fn autonomous_moves_go_through_the_managers_public_api() {
    testkit::proves!("autonomy.movement.runs_through_the_manager");

    let (m, census, ring, _ctx) = setup(&["hot-1"], &[("hot-1", 5)]).await;
    let governor = governor(
        m,
        GovernorMode::FullyAutonomous,
        census,
        Arc::clone(&ring),
        None,
    );
    let report = governor.tick().await.expect("tick");
    assert_eq!(report.promoted, 1, "the census key is hot and moves up");

    // The proof that it went through the manager: the manager's own telemetry
    // (which the governor shares, never replaces) recorded a Promote.
    let framed = framed_key(&ctx_for_tenant("test-tenant"), "hot-1");
    let records = ring.records_for(&framed);
    assert!(
        records.iter().any(|r| r.operation == Operation::Promote),
        "no manager-side Promote record: the move bypassed the public API"
    );
}

#[tokio::test]
async fn only_census_keys_are_moved() {
    testkit::proves!("autonomy.movement.acts_only_on_registered_keys");

    // Two hot entries; only "registered" is in the census. Both are hot, so a
    // governor that moved what it chose would move both.
    let (m, census, ring, ctx) =
        setup(&["registered"], &[("registered", 9), ("unregistered", 9)]).await;
    let governor = governor(m.clone(), GovernorMode::FullyAutonomous, census, ring, None);

    let report = governor.tick().await.expect("tick");
    assert_eq!(report.promoted, 1, "exactly the census key moved");

    assert_eq!(
        m.cachelito()
            .peek(&framed_key(&ctx, "registered"))
            .expect("peek")
            .tier,
        TierId::L0,
        "L1 -> L0: exactly one promote step"
    );
    assert_eq!(
        m.cachelito()
            .peek(&framed_key(&ctx, "unregistered"))
            .expect("peek")
            .tier,
        TierId::L1,
        "a key outside the census is never touched, whatever its heat"
    );
}

#[tokio::test]
async fn aborting_the_spawned_governor_leaves_no_wedged_key() {
    testkit::proves!("autonomy.fully_autonomous_task_is_cancel_safe");

    let (m, census, ring, ctx) = setup(&["loop-1"], &[("loop-1", 1)]).await;
    let governor = governor(m.clone(), GovernorMode::FullyAutonomous, census, ring, None);
    let handle = Arc::new(governor)
        .spawn()
        .expect("a fully-autonomous governor spawns");

    // A few ticks run against the live loop.
    tokio::time::sleep(Duration::from_millis(80)).await;
    handle.abort();

    // The task stops. Its half-finished work -- if any was half-finished --
    // must have unwound through the manager's guards, so every key is usable.
    for i in 0..4 {
        let key = format!("after-abort-{i}");
        m.set(&key, "written after abort".to_string(), &ctx)
            .await
            .expect("a key must be claimable after the loop died");
        let framed = framed_key(&ctx, &key);
        let snapshot = m.cachelito().peek(&framed).expect("peek");
        assert_eq!(
            snapshot.state,
            EntryState::Ready,
            "the cancelled loop must not wedge the entry (iteration {i})"
        );
    }
}

#[tokio::test]
async fn a_cancelled_move_is_resolved_by_the_next_tick() {
    testkit::proves!("autonomy.movement.cancelled_moves_are_resolved_by_the_next_tick");

    let (m, _census, _ring, ctx) = setup(&["movable"], &[]).await;
    // The wreckage of a cancelled move, crafted at the control plane: a `Move`
    // intent on an entry that already existed. This is exactly the state a
    // promote leaves when its future dies between prepare and commit.
    let framed = framed_key(&ctx, "movable");
    let _ = m
        .cachelito()
        .prepare(&framed, None, TierId::L2, IntentKind::Move)
        .expect("craft the move intent");

    let semi = Governor::new(
        m.clone(),
        GovernorSettings {
            mode: GovernorMode::SemiAutonomous,
            recover_after: Some(Duration::ZERO),
            ..GovernorSettings::default()
        },
    );
    let report = semi.tick().await.expect("semi tick");
    assert_eq!(
        report.needs_reconciliation, 1,
        "a Move is retained as evidence and reported, never completed forward"
    );

    // The point of the clause: the key is usable again, not wedged under a
    // claim nobody will release.
    m.set(&"movable".to_string(), "after recovery".to_string(), &ctx)
        .await
        .expect("the cancelled move must not strand the key");
}

#[tokio::test]
async fn the_expiry_sweep_touches_the_control_plane_never_data() {
    testkit::proves!("autonomy.sweeps.expired_sweep_is_control_plane_only");

    let m = governor_manager();
    let ctx = ctx_for_tenant("test-tenant");
    let key = "expiring";
    let value = "still-here".to_string();

    // The control plane carries the TTL; the rung is written TTL-free. The
    // two are decoupled by design -- the stub applies its own expiry to reads,
    // so a tier-side TTL would hide the very bytes this test must see survive.
    {
        let framed = framed_key(&ctx, key);
        let tier = m.bound_tier(&TierId::L1).expect("L1");
        let token = m
            .cachelito()
            .prepare(&framed, None, TierId::L1, IntentKind::Write)
            .expect("prepare");
        tier.set(&KeyRef(&framed), value.clone(), None)
            .await
            .expect("stub stores freely");
        m.cachelito()
            .commit(&token, Some(Duration::from_millis(1)))
            .expect("commit with a control-plane TTL");
    }

    // One millisecond is long past by the time the sweep runs.
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(m.sweep_expired(), 1, "the expired entry is demoted");

    // The rung keeps its bytes. The sweep transitioned control-plane state;
    // whether the bytes stay is the tier's business, decided by nothing here.
    let framed = framed_key(&ctx, key);
    let tier = m.bound_tier(&TierId::L1).expect("L1");
    assert_eq!(
        tier.get(&KeyRef(&framed)).await.expect("stub get"),
        Some(value),
        "the sweep must not touch the data plane"
    );
}

#[tokio::test]
async fn sweeps_are_idempotent_across_ticks() {
    testkit::proves!("autonomy.sweeps.sweeps_are_idempotent");

    let (m, _census, _ring, ctx) = setup(&[], &[]).await;

    // Two abandoned write intents and one expired entry give both sweeps work.
    for key in ["intent-a", "intent-b"] {
        let framed = framed_key(&ctx, key);
        let _ = m
            .cachelito()
            .prepare(&framed, None, TierId::L2, IntentKind::Write)
            .expect("craft intent");
    }
    m.set(
        &"expired-1".to_string(),
        "v".to_string(),
        &ctx.clone().with_ttl(Duration::from_millis(1)),
    )
    .await
    .expect("seed expired");
    tokio::time::sleep(Duration::from_millis(40)).await;

    let semi = Governor::new(
        m.clone(),
        GovernorSettings {
            mode: GovernorMode::SemiAutonomous,
            recover_after: Some(Duration::ZERO),
            ..GovernorSettings::default()
        },
    );

    let first = semi.tick().await.expect("first tick");
    assert_eq!(first.recovered_intents, 2);
    assert_eq!(first.expired_demoted, 1);

    // Tick again on the now-clean ladder: zero work, not errors.
    let second = semi.tick().await.expect("second tick");
    assert_eq!(second.recovered_intents, 0, "recovery is idempotent");
    assert_eq!(second.expired_demoted, 0, "the expiry sweep is idempotent");
}
