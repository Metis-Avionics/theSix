//! Continuity: what a rung is doing, and how a half-finished operation is
//! resolved.
//!
//! The contract's `[continuity]` section asks for five observable states and an
//! idempotent, repeatable recovery. Both live here so the control plane only has
//! to record *what happened* and this module owns *what it means*.
//!
//! # Why recovery has two directions
//!
//! An outstanding commit intent is not a single kind of thing, and resolving all
//! of them the same way is either unsafe or lossy:
//!
//! * A prepared **write** (`set`, population) has a value that is reproducible —
//!   the fetcher can run again. Aborting is safe and honest: the entry reads as
//!   absent and the next read repopulates it. Committing would mean serving a
//!   value whose landing in the tier we never confirmed.
//!
//! * A prepared **move** (`promote`, `demote`) is relocating an *already
//!   committed* value. Aborting it would discard committed data, so it must be
//!   completed forward: read the source if it is still there, otherwise the
//!   destination already has it, then remove the source and commit.
//!
//! Getting this backwards is the difference between "a failed write left no
//! residue" and "a failed promote silently dropped a committed value".

use std::collections::BTreeMap;
use std::time::Duration;

use crate::entry::IntentKind;
use crate::tier::TierId;

/// A rung's condition, as a consumer sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContinuityState {
    /// Serving normally.
    Healthy,
    /// Serving, but with failures recorded and a fallback in play.
    Degraded,
    /// Not serving. Operations fail fast rather than falling through.
    Unavailable,
    /// Coming back after a failure; not yet trusted for new writes.
    Recovering,
    /// Comparing against the authority and repairing drift.
    Reconciling,
}

impl ContinuityState {
    pub const ALL: [ContinuityState; 5] = [
        Self::Healthy,
        Self::Degraded,
        Self::Unavailable,
        Self::Recovering,
        Self::Reconciling,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
            Self::Recovering => "recovering",
            Self::Reconciling => "reconciling",
        }
    }

    /// Whether operations are expected to succeed.
    ///
    /// `Recovering` is deliberately excluded. It is not yet trusted, and a
    /// consumer that treats it as available cannot tell the difference between
    /// "back up" and "never went down" — which is exactly the
    /// `recovering_from_available` distinction the contract requires.
    #[must_use]
    pub fn is_serving(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }

    /// Whether writes should be routed here yet. `Recovering` is serving reads
    /// but not trusted with new data, which is why it is not simply `Healthy`.
    #[must_use]
    pub fn accepts_writes(self) -> bool {
        matches!(self, Self::Healthy | Self::Reconciling)
    }

    /// Parse the contract spelling.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|st| st.name() == s)
    }
}

impl std::fmt::Display for ContinuityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Which way to resolve an outstanding commit intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDirection {
    /// Undo. The value was never committed, so make the entry look untouched.
    Abort,
    /// Finish. The value was already committed elsewhere and is being moved.
    ///
    /// This is the *correct* direction for a move, and it is executable — but
    /// only by a caller that holds the key. The control plane persists a hash, so
    /// a post-crash sweep cannot take this path on its own.
    CompleteForward,
    /// This process cannot resolve it; something outside must.
    ///
    /// The honest answer for a move intent found by a recovery sweep. Completing
    /// it forward means reading the source rung and writing the destination, which
    /// needs the key bytes, and `Cachelito` stores only a hash of them. Aborting is
    /// not a safe substitute either: the move removes the source *before* it
    /// commits, so a crash in that window leaves the value only at the
    /// destination, and aborting would point the control plane at an empty source.
    ///
    /// So the intent is reported and left in place for an external reconciler,
    /// rather than guessed at. Naming this is the point — the previous contract
    /// claimed `complete-forward` for a direction the sweep had no way to execute,
    /// which is a promise the runtime cannot keep.
    ExternalReconciliation,
}

impl RecoveryDirection {
    /// The direction this intent kind must be resolved in, given what this
    /// process knows.
    ///
    /// Not configurable per call site: the kind determines it, and letting a
    /// caller choose would let a prepared move be aborted into data loss.
    ///
    /// A move maps to [`Self::ExternalReconciliation`] rather than to
    /// [`Self::CompleteForward`] even though complete-forward is the right
    /// direction, because resolving it requires the key and only the caller that
    /// still holds it can supply one. A sweep holding nothing but a hash gets the
    /// truthful answer instead of a direction it would have to fake.
    #[must_use]
    pub const fn for_kind(kind: IntentKind) -> Self {
        match kind {
            IntentKind::Write => Self::Abort,
            IntentKind::Move => Self::ExternalReconciliation,
        }
    }

    /// Whether this process can act on the direction without the key.
    #[must_use]
    pub const fn is_executable_here(self) -> bool {
        matches!(self, Self::Abort)
    }

    /// Whether this direction would lose an already-committed value if taken.
    ///
    /// Separate from [`Self::is_executable_here`] on purpose: "I cannot do this
    /// here" and "doing this would destroy data" are different objections, and a
    /// caller that cannot execute a direction still needs to know it is not the
    /// safe fallback.
    #[must_use]
    pub const fn is_success_without_the_key(self) -> bool {
        matches!(self, Self::CompleteForward)
    }
}

/// What one recovery sweep did.
///
/// A named struct rather than a tuple: with three counts a caller has to remember
/// which position is which, and a swap compiles silently. The three are genuinely
/// different outcomes, not variations of one — `needs_reconciliation` in
/// particular is not a failure of the sweep but a statement that the sweep is not
/// the thing that can finish the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// Intents this sweep actually resolved.
    pub recovered: usize,
    /// Intents this sweep tried and could not resolve, for a reason of its own.
    pub failed: usize,
    /// Intents left in place because only something outside this process can
    /// finish them — a move, which needs key bytes the control plane does not
    /// keep.
    pub needs_reconciliation: usize,
}

/// What recovery actually did. Returned rather than logged-and-dropped, because
/// `recovery_failure_must_be_observable` is a contract clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    Aborted {
        kind: IntentKind,
    },
    Completed {
        kind: IntentKind,
    },
    /// Recovery could not resolve this intent; it is still outstanding.
    Failed {
        kind: IntentKind,
    },
}

impl RecoveryOutcome {
    #[must_use]
    pub const fn kind(self) -> IntentKind {
        match self {
            Self::Aborted { kind } | Self::Completed { kind } | Self::Failed { kind } => kind,
        }
    }

    #[must_use]
    pub const fn is_success(self) -> bool {
        !matches!(self, Self::Failed { .. })
    }

    /// Whether this outcome leaves the entry committed.
    #[must_use]
    pub const fn is_committed(self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// A point-in-time view of every rung's continuity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuityReport {
    pub states: BTreeMap<TierId, ContinuityState>,
}

impl ContinuityReport {
    #[must_use]
    pub fn state(&self, tier: TierId) -> ContinuityState {
        self.states
            .get(&tier)
            .copied()
            .unwrap_or(ContinuityState::Unavailable)
    }

    /// Rungs currently serving.
    #[must_use]
    pub fn serving(&self) -> Vec<TierId> {
        self.states
            .iter()
            .filter(|(_, s)| s.is_serving())
            .map(|(t, _)| *t)
            .collect()
    }

    /// Whether every rung is healthy. Useful as a fail-fast startup assertion.
    ///
    /// An empty report is *not* healthy. `all()` over an empty map is vacuously
    /// true, which would let a manager with no tiers report perfect continuity.
    #[must_use]
    pub fn all_healthy(&self) -> bool {
        !self.states.is_empty() && self.states.values().all(|s| *s == ContinuityState::Healthy)
    }

    /// One line per rung, for a log or a panic message. Contains no key or
    /// payload material.
    #[must_use]
    pub fn summary(&self) -> String {
        self.states
            .iter()
            .map(|(t, s)| format!("{t}={s}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Consecutive failures before a rung is considered unavailable.
///
/// Five matches the circuit-breaker threshold that has been in `TierRegistry`
/// since 0.1. It is named here rather than left as a bare literal at the call
/// site, because the number is a policy choice and the contract's
/// `[cia.availability]` section refers to it.
pub const FAILURE_THRESHOLD: u64 = 5;

/// How long an intent must sit unresolved before recovery will touch it.
///
/// Long enough that a commit in flight on a healthy runtime is never mistaken
/// for an abandoned one, short enough that a genuine crash is repaired on the
/// next sweep rather than after the next deploy.
pub const INTENT_RECOVERY_AGE: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_contract_state_is_representable() {
        for s in [
            "healthy",
            "degraded",
            "unavailable",
            "recovering",
            "reconciling",
        ] {
            assert!(ContinuityState::parse(s).is_some(), "{s} is unparseable");
        }
    }

    #[test]
    fn write_intents_abort_and_move_intents_need_the_key() {
        assert_eq!(
            RecoveryDirection::for_kind(IntentKind::Write),
            RecoveryDirection::Abort
        );
        // Not `CompleteForward`, even though that is the correct direction for a
        // move: a sweep holds only a hash, so it cannot execute it. Asserting
        // `CompleteForward` here is what let the contract claim a direction the
        // runtime could not take.
        assert_eq!(
            RecoveryDirection::for_kind(IntentKind::Move),
            RecoveryDirection::ExternalReconciliation
        );
    }

    /// Only abort is executable without the key. This is the property the manager
    /// relies on to decide whether a swept intent is its to resolve.
    #[test]
    fn only_abort_is_executable_without_the_key() {
        assert!(RecoveryDirection::Abort.is_executable_here());
        assert!(!RecoveryDirection::CompleteForward.is_executable_here());
        assert!(!RecoveryDirection::ExternalReconciliation.is_executable_here());
    }

    #[test]
    fn recovering_is_distinguishable_from_available() {
        // A rung that is still recovering must not read as available, or a
        // consumer cannot tell "back up" from "never went down".
        assert!(!ContinuityState::Recovering.is_serving());
        assert!(!ContinuityState::Recovering.accepts_writes());
        assert!(ContinuityState::Healthy.is_serving());
        assert!(ContinuityState::Degraded.is_serving());
        assert!(ContinuityState::Reconciling.accepts_writes());
    }

    #[test]
    fn an_unlisted_rung_reads_as_unavailable() {
        let report = ContinuityReport {
            states: BTreeMap::new(),
        };
        assert_eq!(report.state(TierId::L4), ContinuityState::Unavailable);
        assert!(!report.all_healthy());
        assert_eq!(report.serving().len(), 0);
    }

    #[test]
    fn report_summary_names_every_rung() {
        let mut states = BTreeMap::new();
        for t in TierId::ALL {
            states.insert(t, ContinuityState::Healthy);
        }
        let report = ContinuityReport { states };
        assert!(report.all_healthy());
        assert_eq!(report.serving().len(), TierId::ALL.len());
        assert!(report.summary().contains("L0=healthy"));
    }
}
