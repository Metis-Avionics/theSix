//! Deterministic, seedable fault injection.
//!
//! Gated behind the `faults` feature. A production build of `thesix` does not
//! carry any of this: the crate is compiled without the feature by default, and
//! the fault surface only appears when a caller explicitly asks for it. The
//! test harness (`testkit`) depends on `thesix` with `features = ["faults"]`, so
//! `cargo test` gets the harness automatically while `cargo build` does not pay
//! for it.
//!
//! # Why deterministic
//!
//! A fault test that cannot be reproduced from its seed is a liability: it
//! fails once in CI, passes on re-run, and teaches the team nothing. Every
//! schedule here is derived from a `u64` seed by an explicit PRNG rather than
//! by the platform RNG, and [`FaultPlan`] prints its seed in its `Debug` output
//! so a failing assertion carries the seed needed to replay it.

/// Which tier operation a fault applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Get,
    Set,
    Remove,
    Contains,
}

impl OpKind {
    /// Parse the spelling used in contract and test names.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "get" | "read" => Some(Self::Get),
            "set" | "write" => Some(Self::Set),
            "remove" => Some(Self::Remove),
            "contains" => Some(Self::Contains),
            _ => None,
        }
    }
}

/// The fault taxonomy, one variant per entry in the contract's
/// `[testing.fault_injection] faults` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultClass {
    /// Delay the operation by a fixed amount.
    Latency,
    /// Return `CacheError::Timeout` without doing the work.
    Timeout,
    /// Never resolve. Used to stall the data plane and prove the control plane
    /// stays responsive.
    Hang,
    /// Fail a read.
    ReadFailure,
    /// Fail a write.
    WriteFailure,
    /// Fail metadata reporting (health/capability), not the payload path.
    MetadataFailure,
    /// Corrupt the stored bytes so the integrity check must reject them.
    Corruption,
    /// Drop the connection, modelling a vanished backend.
    Disconnect,
    /// Report the fixed-capacity store as full.
    CapacityExhaustion,
    /// Succeed for the first `n` operations, then fail.
    ///
    /// This names a *schedule*, not an error to produce. A plan that uses it
    /// also names the error to produce once the window closes — see
    /// [`FaultPlan::push_after_n`]. Treating it as the produced class (rather
    /// than as the shape of the schedule) made every scheduled failure fire
    /// immediately, because the class in the plan was never this variant.
    FailureAfterN,
    /// Abort the operation at an await point.
    Cancellation,
}

impl FaultClass {
    /// All classes, in contract order.
    pub const ALL: [FaultClass; 11] = [
        Self::Latency,
        Self::Timeout,
        Self::Hang,
        Self::ReadFailure,
        Self::WriteFailure,
        Self::MetadataFailure,
        Self::Corruption,
        Self::Disconnect,
        Self::CapacityExhaustion,
        Self::FailureAfterN,
        Self::Cancellation,
    ];

    /// The contract spelling, used in test names and in the ledger's `Debug`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Latency => "latency",
            Self::Timeout => "timeout",
            Self::Hang => "hang",
            Self::ReadFailure => "read_failure",
            Self::WriteFailure => "write_failure",
            Self::MetadataFailure => "metadata_failure",
            Self::Corruption => "corruption",
            Self::Disconnect => "disconnect",
            Self::CapacityExhaustion => "capacity_exhaustion",
            Self::FailureAfterN => "failure_after_n_operations",
            Self::Cancellation => "cancellation",
        }
    }

    /// Whether this class is one a `FailureAfterN` schedule can deliver.
    ///
    /// [`Self::FailureAfterN`] itself is excluded: it describes *when* to fail,
    /// not what to return.
    #[must_use]
    pub const fn is_scheduled_failure(self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::ReadFailure
                | Self::WriteFailure
                | Self::MetadataFailure
                | Self::Corruption
                | Self::Disconnect
                | Self::CapacityExhaustion
                | Self::Cancellation
        )
    }

    /// Which operations this fault can legitimately apply to.
    ///
    /// A read failure on a write is meaningless, and arming one anyway would
    /// produce a test that passes for the wrong reason: the write would fail
    /// with an error, and the assertion on the error class would be satisfied
    /// by a fault that was never supposed to apply.
    #[must_use]
    pub fn applies_to(self, op: OpKind) -> bool {
        match self {
            Self::Latency
            | Self::Hang
            | Self::Disconnect
            | Self::Cancellation
            | Self::FailureAfterN => true,
            Self::Timeout | Self::ReadFailure | Self::Corruption => {
                matches!(op, OpKind::Get | OpKind::Contains)
            }
            Self::WriteFailure | Self::CapacityExhaustion => {
                matches!(op, OpKind::Set | OpKind::Remove)
            }
            // Metadata failure is not payload-path-specific: it is the tier's
            // health/capability reporting that is broken, which any operation can
            // observe.
            Self::MetadataFailure => matches!(op, OpKind::Get | OpKind::Set),
        }
    }
}

impl std::fmt::Display for FaultClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for OpKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Get => "get",
            Self::Set => "set",
            Self::Remove => "remove",
            Self::Contains => "contains",
        })
    }
}

/// xorshift64* — small, allocation-free, and identical on every platform.
///
/// Chosen over `rand` because the whole point is reproducibility: a schedule
/// generated here on a maintainer's laptop must be byte-identical to the one
/// generated in CI. `DefaultHasher` is explicitly not stable across releases,
/// so it cannot be used for anything a seed is expected to replay.
#[derive(Debug, Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// Seed the generator. Zero is remapped, because xorshift cannot escape it.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish index into `len` items. `len` must be non-zero.
    pub fn below(&mut self, len: usize) -> usize {
        debug_assert!(len > 0, "below(0) would be a division by zero");
        (self.next_u64() % len as u64) as usize
    }
}

/// One armed fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmedFault {
    pub op: OpKind,
    pub class: FaultClass,
    /// For `Latency`: how long to stall. For `FailureAfterN`: how many
    /// operations succeed before the fault starts firing.
    pub amount: u64,
}

/// A deterministic schedule of faults.
///
/// `FaultPlan` is a queue: the first armed fault whose `op` matches an incoming
/// operation fires, and is consumed. Once the queue is empty the tier behaves
/// normally. That makes a plan a script — "the next set fails, the one after it
/// succeeds" — which is what a fault test actually wants to express, and what a
/// random-probability injector cannot express.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    seed: u64,
    queue: Vec<ArmedFault>,
}

impl FaultPlan {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A plan derived from a seed. Replaying the seed replays the schedule.
    #[must_use]
    pub fn from_seed(seed: u64, len: usize) -> Self {
        let mut rng = DeterministicRng::new(seed);
        let mut queue = Vec::with_capacity(len);
        for _ in 0..len {
            let class = FaultClass::ALL[rng.below(FaultClass::ALL.len())];
            // Pick an operation the class can actually apply to, so a
            // generated plan cannot contain a no-op fault.
            let candidates: &[OpKind] = match class {
                FaultClass::MetadataFailure => &[OpKind::Get, OpKind::Set],
                FaultClass::ReadFailure | FaultClass::Corruption | FaultClass::Timeout => {
                    &[OpKind::Get, OpKind::Contains]
                }
                FaultClass::WriteFailure | FaultClass::CapacityExhaustion => {
                    &[OpKind::Set, OpKind::Remove]
                }
                _ => &[OpKind::Get, OpKind::Set, OpKind::Remove, OpKind::Contains],
            };
            let op = candidates[rng.below(candidates.len())];
            queue.push(ArmedFault {
                op,
                class,
                amount: match class {
                    FaultClass::Latency => 1 + rng.below(50) as u64,
                    FaultClass::FailureAfterN => rng.below(3) as u64,
                    _ => 0,
                },
            });
        }
        Self { seed, queue }
    }

    /// Arm one fault.
    #[must_use]
    pub fn push(mut self, op: OpKind, class: FaultClass) -> Self {
        self.queue.push(ArmedFault {
            op,
            class,
            amount: 0,
        });
        self
    }

    /// Arm a latency fault in milliseconds.
    #[must_use]
    pub fn push_latency_ms(mut self, op: OpKind, ms: u64) -> Self {
        self.queue.push(ArmedFault {
            op,
            class: FaultClass::Latency,
            amount: ms,
        });
        self
    }

    /// Arm a "succeed `n` times, then fail" schedule.
    ///
    /// `class` is the error produced *after* the window closes, not
    /// [`FaultClass::FailureAfterN`]: that variant names the schedule, and
    /// naming it here would leave the harness with nothing to return once the
    /// window is spent.
    #[must_use]
    pub fn push_after_n(mut self, op: OpKind, class: FaultClass, n: u64) -> Self {
        debug_assert!(
            class.is_scheduled_failure(),
            "{} is not an error a scheduled failure can produce",
            class.as_str()
        );
        self.queue.push(ArmedFault {
            op,
            class,
            amount: n,
        });
        self
    }

    /// Put a fault back at the head of the queue.
    ///
    /// Used by `FailureAfterN`: the fault stays armed while the grace window is
    /// open, and once the window closes it fires. Re-queueing rather than
    /// counting in the tier keeps the schedule inspectable from the plan alone.
    pub fn rearm(&mut self, fault: ArmedFault) {
        self.queue.insert(0, fault);
    }

    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Take the first armed fault for `op`, if any.
    pub fn take_for(&mut self, op: OpKind) -> Option<ArmedFault> {
        let idx = self.queue.iter().position(|f| f.op == op)?;
        Some(self.queue.remove(idx))
    }
}

/// Per-class activation counters.
///
/// This is the anti-vacuity instrument. A test that arms a hang and asserts the
/// operation timed out has proven nothing unless it also shows the hang fired;
/// a test whose tier returned an error for an unrelated reason would pass. Every
/// fault test asserts a counter from here.
#[derive(Debug, Default)]
pub struct FaultLedger {
    counts: [std::sync::atomic::AtomicU64; 11],
    ops: std::sync::atomic::AtomicU64,
}

impl FaultLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, class: FaultClass) {
        let idx = Self::index(class);
        self.counts[idx].fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many times `class` actually fired.
    #[must_use]
    pub fn fired(&self, class: FaultClass) -> u64 {
        self.counts[Self::index(class)].load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Total operations observed, armed or not.
    #[must_use]
    pub fn ops_observed(&self) -> u64 {
        self.ops.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn observe_op(&self) {
        self.ops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Classes that were armed but never fired — the usual signature of a test
    /// that asserts an error it never caused.
    #[must_use]
    pub fn never_fired(&self, armed: &[FaultClass]) -> Vec<&'static str> {
        armed
            .iter()
            .filter(|c| self.fired(**c) == 0)
            .map(|c| c.as_str())
            .collect()
    }

    const fn index(class: FaultClass) -> usize {
        match class {
            FaultClass::Latency => 0,
            FaultClass::Timeout => 1,
            FaultClass::Hang => 2,
            FaultClass::ReadFailure => 3,
            FaultClass::WriteFailure => 4,
            FaultClass::MetadataFailure => 5,
            FaultClass::Corruption => 6,
            FaultClass::Disconnect => 7,
            FaultClass::CapacityExhaustion => 8,
            FaultClass::FailureAfterN => 9,
            FaultClass::Cancellation => 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_reproducible_from_seed() {
        let a: Vec<u64> = {
            let mut r = DeterministicRng::new(42);
            (0..8).map(|_| r.next_u64()).collect()
        };
        let b: Vec<u64> = {
            let mut r = DeterministicRng::new(42);
            (0..8).map(|_| r.next_u64()).collect()
        };
        assert_eq!(a, b, "same seed must produce the same stream");
        let c: Vec<u64> = {
            let mut r = DeterministicRng::new(43);
            (0..8).map(|_| r.next_u64()).collect()
        };
        assert_ne!(a, c, "different seeds must diverge");
    }

    #[test]
    fn zero_seed_does_not_lock_the_generator() {
        let mut r = DeterministicRng::new(0);
        let first = r.next_u64();
        let second = r.next_u64();
        assert_ne!(first, second, "xorshift must escape a zero state");
    }

    #[test]
    fn generated_plans_only_arm_applicable_faults() {
        for seed in 0..500_u64 {
            let plan = FaultPlan::from_seed(seed, 8);
            assert_eq!(plan.seed(), seed);
            let mut armed = plan.clone();
            while let Some(f) = armed.take_for(OpKind::Get) {
                assert!(
                    f.class.applies_to(OpKind::Get),
                    "seed {seed} armed {} for a read, which can never fire",
                    f.class.as_str()
                );
            }
            let mut armed = plan.clone();
            while let Some(f) = armed.take_for(OpKind::Set) {
                assert!(
                    f.class.applies_to(OpKind::Set),
                    "seed {seed} armed {} for a write, which can never fire",
                    f.class.as_str()
                );
            }
        }
    }

    #[test]
    fn ledger_reports_armed_but_never_fired() {
        let ledger = FaultLedger::new();
        ledger.record(FaultClass::Hang);
        let missed = ledger.never_fired(&[FaultClass::Hang, FaultClass::Corruption]);
        assert_eq!(missed, vec!["corruption"]);
    }

    #[test]
    fn queue_consumes_in_order_and_then_stops() {
        let mut plan = FaultPlan::new()
            .push(OpKind::Set, FaultClass::WriteFailure)
            .push(OpKind::Set, FaultClass::WriteFailure);
        assert_eq!(
            plan.take_for(OpKind::Set).unwrap().class,
            FaultClass::WriteFailure
        );
        assert_eq!(
            plan.take_for(OpKind::Set).unwrap().class,
            FaultClass::WriteFailure
        );
        assert!(plan.take_for(OpKind::Set).is_none());
        assert!(plan.take_for(OpKind::Get).is_none(), "other ops unaffected");
    }
}
