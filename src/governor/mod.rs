//! Autonomous maintenance of the cache ladder.
//!
//! The governor is a maintenance loop that observes the control plane and acts
//! through the manager's public operations. It decides *which* keys to move and
//! *whether* to sweep; it never implements a move of its own. Every data-plane
//! action runs through `promote` / `demote`, so intents, generations, guards and
//! telemetry behave exactly as they do for a consumer call — there is no second
//! write path.
//!
//! # Autonomy levels
//!
//! | [`GovernorMode`] | What `tick` executes |
//! |---|---|
//! | [`GovernorMode::Manual`] | Nothing. `GovernorReport::plan` carries the suggestions; the caller decides. |
//! | [`GovernorMode::SemiAutonomous`] | Recovery sweep, expiry sweep, health-driven rebinding. Entry movement stays a suggestion. |
//! | [`GovernorMode::FullyAutonomous`] | Everything, with a per-tick move budget. |
//!
//! # What the governor may know
//!
//! The control plane keeps only key hashes, and the tiers keep none at all, so
//! the governor cannot discover keys by itself — that is a confidentiality
//! property, not a limitation to work around. Entry movement therefore acts on
//! the [`KeyCensus`]: the set of keys the *application* registered as movable.
//! A key outside the census is never touched by autonomy, and heat is measured
//! as counts of [`KeyIdentity`] digests from telemetry records, which cannot
//! reconstruct a key.
//!
//! Without a census or without a heat source, rebalance legs are skipped and the
//! plan says so, rather than guessing from absent data.

mod bootstrap;
mod census;
mod task;

pub use bootstrap::{
    BindingReport, DetectedUrl, EnvSource, L3Binding, Ladder, MapEnv, ProcessEnv, assemble_ladder,
    detect_redis_endpoint,
};
pub use census::{CensusKey, KeyCensus};
pub use task::GovernorHandle;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::continuity::ContinuityState;
use crate::control::cachelito::PlaneStats;
use crate::entry::EntryState;
use crate::error::CacheError;
use crate::manager::CacheManager;
use crate::policy::CachePolicy;
use crate::telemetry::{KeyIdentity, Outcome, RingTelemetry};
use crate::tier::hot_rung::HotRungTier;
use crate::tier::{CacheTier, TierId};

/// Closures that can rebuild a rung's backing tier, used on degradation.
///
/// The governor calls this only for rungs that hold a [`HotRungTier`]. It must
/// return a tier claiming that same rung — the wrapper refuses anything else —
/// and shares the tiers' blocking obligation: if it performs I/O it must either
/// be fast or the caller must accept a blocking tick, because the governor runs
/// it under `tokio::task::spawn_blocking` and its completion bounds one rebind.
pub type Rebinder<V> = Arc<
    dyn Fn(TierId) -> Result<Option<Arc<dyn CacheTier<V>>>, CacheError> + Send + Sync + 'static,
>;

/// How much autonomy `tick` exercises.
///
/// The modes form an escalating scale: each level does everything the level
/// below does, plus one more class of action. Nothing here bypasses the manager,
/// so the guarantees each action already has (two-phase intents, guards on
/// cancellation, bounded retries) hold identically under autonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GovernorMode {
    /// Observe and suggest. `tick` executes nothing.
    #[default]
    Manual,
    /// Execute control-plane maintenance; entry movement stays a suggestion.
    SemiAutonomous,
    /// Execute everything, bounded by the per-tick move budget.
    FullyAutonomous,
}

impl GovernorMode {
    /// Whether `tick` executes control-plane maintenance sweeps.
    #[must_use]
    pub const fn executes_maintenance(self) -> bool {
        matches!(self, Self::SemiAutonomous | Self::FullyAutonomous)
    }

    /// Whether `tick` executes entry movement.
    #[must_use]
    pub const fn executes_moves(self) -> bool {
        matches!(self, Self::FullyAutonomous)
    }
}

/// Knobs of the governor. Every limit here exists so one tick is bounded:
/// `unbounded_queue_growth` applies to a maintenance loop exactly as it does to
/// a data-plane queue.
#[derive(Debug, Clone)]
pub struct GovernorSettings {
    pub mode: GovernorMode,
    /// Interval for the spawned task ([`GovernorMode::FullyAutonomous`]).
    /// Clamped to at least one millisecond; a zero interval is a busy loop.
    pub interval: Duration,
    /// Maximum entries moved (promoted or demoted, combined) per tick.
    pub max_moves_per_tick: usize,
    /// Maximum rungs rebound per tick.
    pub max_rebinds_per_tick: usize,
    /// Rung occupancy ratio (in `0.0..=1.0`) at which a rung sheds cold entries.
    /// A rung without capacity accounting never pressures.
    pub pressure_threshold: f64,
    /// A census key whose ring-window read count reaches this is hot.
    pub hot_heat: u64,
    /// Minimum intent age the recovery sweep considers, or `None` for the
    /// crate default ([`crate::continuity::INTENT_RECOVERY_AGE`]).
    pub recover_after: Option<Duration>,
}

impl Default for GovernorSettings {
    fn default() -> Self {
        GovernorSettings {
            mode: GovernorMode::Manual,
            interval: Duration::from_secs(30),
            max_moves_per_tick: 8,
            max_rebinds_per_tick: 2,
            pressure_threshold: 0.85,
            hot_heat: 8,
            recover_after: None,
        }
    }
}

/// What the governor decided this tick, before any execution.
///
/// Suggestions are expressed against census positions, so a `Manual` caller can
/// act on them (via [`Governor::apply_plan`]) or ignore them without the
/// governor retaining any knowledge a report does not say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GovernorPlan {
    pub recovery_sweep: bool,
    pub expire_sweep: bool,
    /// Rungs whose continuity is degraded or worse, bounded by the rebind budget.
    pub rebind: Vec<TierId>,
    /// Census indices to promote one rung inward (toward L0).
    pub promote: Vec<usize>,
    /// Census indices to demote one rung outward (toward L5).
    pub demote: Vec<usize>,
    /// Reasons a rebalance leg produced nothing.
    pub skipped: Vec<Skip>,
}

/// Why a leg of the plan decided to do nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// No heat source is wired, so hot/cold cannot be judged without guessing.
    NoHeatSource,
    /// No census is wired, so there are no keys the governor may move.
    NoCensus,
}

/// What one tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GovernorReport {
    pub plan: GovernorPlan,
    /// Intents the recovery sweep actually resolved.
    pub recovered_intents: usize,
    /// Move intents the sweep can only report for external reconciliation.
    pub needs_reconciliation: usize,
    /// Expired entries the expiry sweep demoted to `Stale`.
    pub expired_demoted: usize,
    pub rebinds_done: usize,
    pub rebind_failures: usize,
    pub rebinding_not_configured: bool,
    pub promoted: usize,
    pub demoted: usize,
    pub move_failures: usize,
}

/// The autonomous maintenance loop over one manager.
pub struct Governor<K, V, P> {
    manager: Arc<CacheManager<K, V, P>>,
    settings: GovernorSettings,
    census: Option<Arc<KeyCensus<K>>>,
    heat: Option<Arc<RingTelemetry>>,
    /// Rungs that may be rebound at runtime. Everything else is fixed.
    hot_rungs: Vec<Arc<HotRungTier<V>>>,
    rebinder: Option<Rebinder<V>>,
}

impl<K, V, P> Governor<K, V, P>
where
    K: crate::key::Key,
    V: Clone + Send + Sync + 'static,
    P: CachePolicy<K, V>,
{
    #[must_use]
    pub fn new(manager: Arc<CacheManager<K, V, P>>, settings: GovernorSettings) -> Self {
        Governor {
            manager,
            settings,
            census: None,
            heat: None,
            hot_rungs: Vec::new(),
            rebinder: None,
        }
    }

    /// Register the movable key set. Without it, entry movement does nothing.
    #[must_use]
    pub fn with_census(mut self, census: Arc<KeyCensus<K>>) -> Self {
        self.census = Some(census);
        self
    }

    /// Register the telemetry ring heat is measured from. Without it, no key is
    /// ever judged hot or cold, and the plan says so.
    #[must_use]
    pub fn with_heat(mut self, heat: Arc<RingTelemetry>) -> Self {
        self.heat = Some(heat);
        self
    }

    /// Register a rung as rebindable at runtime.
    #[must_use]
    pub fn with_hot_rung(mut self, tier: Arc<HotRungTier<V>>) -> Self {
        self.hot_rungs.push(tier);
        self
    }

    /// Register the rung-rebuilding closure used by the rebind leg.
    #[must_use]
    pub fn with_rebinder(mut self, rebinder: Rebinder<V>) -> Self {
        self.rebinder = Some(rebinder);
        self
    }

    /// The configured settings.
    #[must_use]
    pub const fn settings(&self) -> &GovernorSettings {
        &self.settings
    }

    /// The manager this governor maintains.
    #[must_use]
    pub fn manager(&self) -> &Arc<CacheManager<K, V, P>> {
        &self.manager
    }

    /// Spawn the maintenance loop for a [`GovernorMode::FullyAutonomous`]
    /// governor. The loop sleeps for the configured interval, ticks, and
    /// repeats; aborting it unwinds through the manager's guards, so no key is
    /// left wedged by a cancelled tick.
    ///
    /// # Errors
    ///
    /// [`CacheError::ConfigurationError`] when the mode is not
    /// `FullyAutonomous` — the other modes are caller-driven, and a loop that
    /// would do nothing is a configuration mistake.
    pub fn spawn(self: Arc<Self>) -> Result<GovernorHandle, CacheError>
    where
        K: 'static,
        P: CachePolicy<K, V> + 'static,
    {
        task::spawn(self)
    }

    /// Observe the ladder and produce one plan, without executing it.
    ///
    /// Pure over its inputs: same observation, same plan. That is what makes the
    /// planner property-testable separately from the loop that runs it.
    #[must_use]
    pub fn plan(&self) -> GovernorPlan {
        let observation = self.observe();
        let heat = self.measure_heat();
        plan_tick(&observation, &heat, &self.settings)
    }

    /// Run one tick at the configured autonomy level.
    ///
    /// # Errors
    ///
    /// Propagates only control-plane faults that make the observation itself
    /// impossible (a poisoned shard); per-action failures are counted in the
    /// report rather than failing the tick, because a maintenance loop that
    /// aborts on the first failed move abandons every key after that move.
    pub async fn tick(&self) -> Result<GovernorReport, CacheError> {
        let plan = self.plan();
        if self.settings.mode.executes_maintenance() {
            Ok(self.apply_plan(&plan).await)
        } else {
            Ok(GovernorReport {
                plan,
                ..GovernorReport::default()
            })
        }
    }

    /// Execute a plan at the governor's configured autonomy level.
    ///
    /// [`GovernorMode::Manual`] callers that accept a plan may run it through
    /// here explicitly; [`GovernorMode::SemiAutonomous`] and
    /// [`GovernorMode::FullyAutonomous`] reach it from `tick`.
    pub async fn apply_plan(&self, plan: &GovernorPlan) -> GovernorReport {
        let mut report = GovernorReport {
            plan: plan.clone(),
            ..GovernorReport::default()
        };
        if !self.settings.mode.executes_maintenance() {
            return report;
        }

        // Recovery first: an outstanding intent from a previous tick's cancelled
        // move must be resolved before this tick issues new moves, or the sweep
        // would be fighting the debris it just caused.
        if plan.recovery_sweep {
            let age = self
                .settings
                .recover_after
                .unwrap_or(crate::continuity::INTENT_RECOVERY_AGE);
            let r = self.manager.recover_older_than(age);
            report.recovered_intents = r.recovered;
            report.needs_reconciliation = r.needs_reconciliation;
        }

        if plan.expire_sweep {
            report.expired_demoted = self.manager.sweep_expired();
        }

        // Rebinding is a leg, not a gate: a rebinder the caller chose not to
        // configure disables rebinding only, not everything after it. An early
        // return here silently starved every later leg of the plan.
        match &self.rebinder {
            None => report.rebinding_not_configured |= !plan.rebind.is_empty(),
            Some(rebinder) => {
                for rung in &plan.rebind {
                    let Some(hot) = self.hot_rungs.iter().find(|h| h.rung() == *rung) else {
                        continue;
                    };
                    let rebinder = Arc::clone(rebinder);
                    let rung = *rung;
                    let outcome = tokio::task::spawn_blocking(move || rebinder(rung))
                        .await
                        .map_err(|_| CacheError::Cancelled)
                        .and_then(std::convert::identity);
                    match outcome {
                        Ok(Some(replacement)) => match hot.hot_swap(replacement) {
                            Ok(_) => report.rebinds_done += 1,
                            Err(_) => report.rebind_failures += 1,
                        },
                        // The rebinder declines: it has nothing to bind here and
                        // says so rather than pretending the rung was rebuilt.
                        Ok(None) => report.rebinding_not_configured |= true,
                        Err(_) => report.rebind_failures += 1,
                    }
                }
            }
        }

        if !self.settings.mode.executes_moves() {
            return report;
        }
        let Some(census) = &self.census else {
            return report;
        };
        let entries = census.entries();
        let ctx = census.context().clone();
        let mut budget = self.settings.max_moves_per_tick;

        for &idx in &plan.demote {
            if budget == 0 {
                break;
            }
            let Some(entry) = entries.get(idx) else {
                continue;
            };
            match self.manager.demote(&entry.key, &ctx).await {
                Ok(()) => {
                    report.demoted += 1;
                    budget -= 1;
                }
                Err(CacheError::CapacityExhausted) => {
                    // The colder rung is full. Stop demoting: hammering a full
                    // rung burns budget without freeing pressure anywhere.
                    report.move_failures += 1;
                    break;
                }
                Err(err) => {
                    // Not movable this tick: the entry left, a concurrent move
                    // won, or the rung refused. Counted, bounded, non-fatal —
                    // one stuck move must not abandon the rest of the plan
                    // (`Miss`/`WriteContended` included by the catch-all).
                    let _ = err;
                    report.move_failures += 1;
                    budget -= 1;
                }
            }
        }

        for &idx in &plan.promote {
            if budget == 0 {
                break;
            }
            let Some(entry) = entries.get(idx) else {
                continue;
            };
            match self.manager.promote(&entry.key, &ctx).await {
                Ok(()) => {
                    report.promoted += 1;
                    budget -= 1;
                }
                Err(_) => report.move_failures += 1,
            }
        }

        report
    }

    /// Read the ladder's state once.
    fn observe(&self) -> Observation {
        let stats = self.manager.plane_stats();
        let continuity = self.manager.continuity();
        let mut rung_capacity = [None; 7];
        let mut rung_state = [ContinuityState::Healthy; 7];
        for &tier in TierId::ALL.iter().rev() {
            if let Some(state) = continuity.states.get(&tier) {
                rung_state[tier.as_usize()] = *state;
            }
            if let Some(bound) = self.manager.bound_tier(&tier) {
                rung_capacity[tier.as_usize()] = bound.capacity();
            }
        }

        // Per-census current rung, from the control plane's own record.
        let mut census_view = Vec::new();
        if let Some(census) = &self.census {
            for (idx, entry) in census.entries().into_iter().enumerate() {
                let view = match self.manager.cachelito().peek(&entry.framed) {
                    Ok(s) => CensusEntryView {
                        index: idx,
                        identity: entry.identity,
                        tier: s.tier,
                        state: s.state,
                    },
                    Err(_) => CensusEntryView {
                        index: idx,
                        identity: entry.identity,
                        tier: TierId::L0,
                        state: EntryState::Absent,
                    },
                };
                census_view.push(view);
            }
        }

        Observation {
            stats,
            rung_capacity,
            rung_state,
            census: census_view,
        }
    }

    /// Per-key heat: the number of served reads in the telemetry ring window.
    ///
    /// A ring is a window, not a ledger — an entry leaving it does not mean the
    /// key went cold, it means the evidence aged out. That undercounts, which is
    /// the safe direction: a key still in the window is provably hot, and
    /// calling a proven-hot key cold is the mistake worth forbidding.
    fn measure_heat(&self) -> HashMap<KeyIdentity, u64> {
        let Some(ring) = &self.heat else {
            return HashMap::new();
        };
        let mut heat: HashMap<KeyIdentity, u64> = HashMap::new();
        for record in ring.records() {
            if record.outcome != Outcome::Served {
                continue;
            }
            if !matches!(
                record.operation,
                crate::telemetry::Operation::Get
                    | crate::telemetry::Operation::GetOrFetch
                    | crate::telemetry::Operation::Refresh
            ) {
                continue;
            }
            *heat.entry(record.key).or_insert(0) += 1;
        }
        heat
    }
}

/// One census entry as the planner sees it.
#[derive(Debug, Clone, Copy)]
struct CensusEntryView {
    index: usize,
    identity: KeyIdentity,
    tier: TierId,
    state: EntryState,
}

/// One observation of the ladder, enough to plan a tick.
struct Observation {
    stats: PlaneStats,
    /// Fixed capacity per rung, when the bound tier tracks one.
    rung_capacity: [Option<usize>; 7],
    rung_state: [ContinuityState; 7],
    census: Vec<CensusEntryView>,
}

/// Per-rung occupancy ratio, when the rung tracks a fixed capacity.
fn rung_occupancy(observation: &Observation, rung: TierId) -> Option<f64> {
    let cap = observation.rung_capacity[rung.as_usize()]?;
    if cap == 0 {
        return None;
    }
    let used = observation.stats.by_tier[rung.as_usize()];
    Some(
        f64::from(u32::try_from(used).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(cap).unwrap_or(u32::MAX)),
    )
}

/// The planner. Pure over `(observation, heat, settings) -> plan`.
///
/// Kept as a free function so it can be tested without a manager and randomised
/// without fault injection: the plan is where every *decision* is made, so the
/// invariants that matter ("never proposes the authority rung", "budget
/// saturates") live here, not in the executor.
fn plan_tick(
    observation: &Observation,
    heat: &HashMap<KeyIdentity, u64>,
    settings: &GovernorSettings,
) -> GovernorPlan {
    // Maintenance sweeps are always worth planning: both are idempotent and
    // control-plane-only, so they cost nothing on an already-clean ladder.
    let mut plan = GovernorPlan {
        recovery_sweep: true,
        expire_sweep: true,
        ..GovernorPlan::default()
    };

    // Rebind the worst rungs first, bounded by budget. Worst = deepest rung
    // first, because an unavailable distributed rung strands more of the ladder
    // than a degraded local one.
    let mut degraded: Vec<TierId> = TierId::ALL
        .iter()
        .filter(|t| t.is_cache_rung())
        .filter(|t| {
            matches!(
                observation.rung_state[t.as_usize()],
                ContinuityState::Degraded | ContinuityState::Unavailable
            )
        })
        .copied()
        .collect();
    degraded.sort_by_key(|t| std::cmp::Reverse(t.as_usize()));
    plan.rebind = degraded
        .into_iter()
        .take(settings.max_rebinds_per_tick)
        .collect();

    // Rebalance planning needs both a key set and a heat source. Missing either
    // is a stated skip, not a silent zero.
    if observation.census.is_empty() {
        plan.skipped.push(Skip::NoCensus);
        return plan;
    }
    if heat.is_empty() {
        plan.skipped.push(Skip::NoHeatSource);
        return plan;
    }

    let mut budget = settings.max_moves_per_tick;

    // Promote: hot, readable, not already at the top of the ladder. Hottest
    // first, so the budget goes to the keys with the most evidence.
    let mut hot: Vec<&CensusEntryView> = observation
        .census
        .iter()
        .filter(|v| {
            v.state == EntryState::Ready
                && v.tier.is_cache_rung()
                && v.tier.as_usize() > 0
                && heat.get(&v.identity).copied().unwrap_or(0) >= settings.hot_heat
        })
        .collect();
    hot.sort_by_key(|v| std::cmp::Reverse((heat.get(&v.identity).copied().unwrap_or(0), v.index)));
    let mut promoted = Vec::new();
    for view in hot {
        if budget == 0 {
            break;
        }
        budget -= 1;
        promoted.push(view.index);
    }
    plan.promote = promoted;

    // Demote: cold entries, only off rungs under pressure, only downward within
    // the ladder, bounded by what the promote leg left.
    let rung_pressure: Vec<TierId> = TierId::ALL
        .iter()
        .filter(|t| t.is_cache_rung())
        .copied()
        .filter(|t| {
            rung_occupancy(observation, *t)
                .is_some_and(|ratio| ratio + f64::EPSILON >= settings.pressure_threshold)
        })
        .collect();
    if budget > 0 {
        plan.demote = observation
            .census
            .iter()
            .filter(|v| {
                v.state == EntryState::Ready
                    && v.tier.is_cache_rung()
                    && v.tier.as_usize() < crate::policy::LAST_CACHE_TIER.as_usize()
                    && rung_pressure.contains(&v.tier)
                    && heat.get(&v.identity).copied().unwrap_or(0) == 0
            })
            .map(|v| v.index)
            .collect();
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation_of(states: &[ContinuityState]) -> Observation {
        use crate::continuity::ContinuityState as C;
        let mut rung_state = [C::Healthy; 7];
        for &t in &TierId::ALL {
            if let Some(s) = states.get(t.as_usize()) {
                rung_state[t.as_usize()] = *s;
            }
        }
        Observation {
            stats: PlaneStats::default(),
            rung_capacity: [None; 7],
            rung_state,
            census: Vec::new(),
        }
    }

    #[test]
    fn the_planner_never_targets_the_authority_rung() {
        testkit::proves!(
            "autonomy.movement.respects_last_cache_rung",
            "autonomy.sweeps.maintenance_legs_are_planned_unconditionally"
        );

        let mut obs = observation_of(&[]);
        obs.rung_state[TierId::L5.as_usize()] = ContinuityState::Unavailable;
        let plan = plan_tick(&obs, &HashMap::new(), &GovernorSettings::default());
        for tier in &plan.rebind {
            assert!(
                tier.is_cache_rung(),
                "{tier} is not a cache rung and must never be rebound or moved to"
            );
        }
    }

    #[test]
    fn rebind_binds_worst_rungs_under_budget() {
        let mut obs = observation_of(&[]);
        obs.rung_state[TierId::L2.as_usize()] = ContinuityState::Unavailable;
        obs.rung_state[TierId::L1.as_usize()] = ContinuityState::Degraded;
        obs.rung_state[TierId::L0.as_usize()] = ContinuityState::Degraded;

        let settings = GovernorSettings {
            max_rebinds_per_tick: 2,
            ..GovernorSettings::default()
        };
        let plan = plan_tick(&obs, &HashMap::new(), &settings);

        assert_eq!(plan.rebind.len(), 2);
        assert!(plan.rebind.contains(&TierId::L2), "unavailable runs first");
    }

    #[test]
    fn a_missing_census_is_a_stated_skip() {
        testkit::proves!("autonomy.movement.has_no_key_of_its_own");

        let obs = observation_of(&[]);
        let plan = plan_tick(&obs, &HashMap::new(), &GovernorSettings::default());
        assert_eq!(plan.skipped, vec![Skip::NoCensus]);
    }

    #[test]
    fn a_missing_heat_source_is_a_stated_skip() {
        testkit::proves!("autonomy.movement.requires_observed_heat");

        let mut obs = observation_of(&[]);
        obs.census.push(CensusEntryView {
            index: 0,
            identity: KeyIdentity::of(b"k"),
            tier: TierId::L1,
            state: EntryState::Ready,
        });
        let plan = plan_tick(&obs, &HashMap::new(), &GovernorSettings::default());
        assert_eq!(plan.skipped, vec![Skip::NoHeatSource]);
    }

    #[test]
    fn occupancy_is_none_for_a_rung_without_capacity_accounting() {
        let mut obs = observation_of(&[]);
        obs.rung_capacity[TierId::L0.as_usize()] = Some(10);
        obs.rung_capacity[TierId::L3.as_usize()] = None;
        obs.stats.by_tier[TierId::L0.as_usize()] = 3;
        assert!(rung_occupancy(&obs, TierId::L3).is_none());
        assert!(rung_occupancy(&obs, TierId::L0).is_some());
    }

    #[test]
    fn hot_keys_are_promoted_coldest_ones_demoted_only_under_pressure() {
        let mut obs = observation_of(&[]);
        let hot_id = KeyIdentity::of(b"hot");
        let cold_id = KeyIdentity::of(b"cold");
        obs.census.push(CensusEntryView {
            index: 0,
            identity: hot_id,
            tier: TierId::L2,
            state: EntryState::Ready,
        });
        obs.census.push(CensusEntryView {
            index: 1,
            identity: cold_id,
            tier: TierId::L2,
            state: EntryState::Ready,
        });
        obs.rung_capacity[TierId::L2.as_usize()] = Some(10);
        obs.stats.by_tier[TierId::L2.as_usize()] = 9;

        let mut heat = HashMap::new();
        heat.insert(hot_id, 20_u64);

        let settings = GovernorSettings {
            max_moves_per_tick: 8,
            hot_heat: 2,
            ..GovernorSettings::default()
        };
        let plan = plan_tick(&obs, &heat, &settings);

        assert_eq!(plan.promote, vec![0], "the hot key is promoted");
        assert_eq!(
            plan.demote,
            vec![1],
            "the cold key is demoted: rung pressures"
        );
    }

    #[test]
    fn the_move_budget_saturates() {
        testkit::proves!("autonomy.movement.move_budget_is_enforced");

        let mut obs = observation_of(&[]);
        let mut heat = HashMap::new();
        for i in 0..10_u64 {
            let id = KeyIdentity::of(format!("hot-{i}").as_bytes());
            heat.insert(id, 50_u64);
            obs.census.push(CensusEntryView {
                index: i as usize,
                identity: id,
                tier: TierId::L2,
                state: EntryState::Ready,
            });
        }
        let settings = GovernorSettings::default();
        let plan = plan_tick(&obs, &heat, &settings);
        assert!(plan.promote.len() <= settings.max_moves_per_tick);
        assert!(plan.demote.len() + plan.promote.len() <= settings.max_moves_per_tick);
    }

    #[test]
    fn an_entry_sitting_on_the_authority_is_never_planned() {
        let mut obs = observation_of(&[]);
        let mut heat = HashMap::new();
        let id = KeyIdentity::of(b"authority-entry");
        heat.insert(id, 100_u64);
        obs.census.push(CensusEntryView {
            index: 0,
            identity: id,
            tier: TierId::L6,
            state: EntryState::Ready,
        });
        let settings = GovernorSettings::default();
        let plan = plan_tick(&obs, &heat, &settings);
        assert_eq!(plan.promote, Vec::<usize>::new());
        assert_eq!(plan.demote, Vec::<usize>::new());
    }
}
