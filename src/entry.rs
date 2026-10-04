use std::time::Duration;

/// Monotonic version of an entry's committed value.
///
/// Generations are the only thing standing between a slow population and a
/// stale write, so the comparison discipline matters as much as the type.
/// `Cachelito::publish` requires *exact equality* with the current generation,
/// not merely that the caller is not behind: an exact check also rejects a
/// caller that somehow got ahead, which a `<` check would accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

impl Generation {
    #[must_use]
    pub fn new(value: u64) -> Self {
        Generation(value)
    }

    /// Whether `self` is older than `current`.
    #[must_use]
    pub fn is_stale(&self, current: Generation) -> bool {
        self.0 < current.0
    }
}

impl std::fmt::Display for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The control-plane state of one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryState {
    /// Nothing committed.
    Absent,
    /// A value is committed and readable.
    Ready,
    /// Committed but past its TTL; readable as stale, not as fresh.
    Stale,
    /// A population is running. Exactly one caller owns it.
    InFlight,
    /// A write reached `prepare` and has not reached `commit` or `abort`.
    ///
    /// This state exists so an uncommitted write is *representable*. Without it
    /// a two-phase commit has to either hide the intermediate state (so partial
    /// writes become indistinguishable from absent ones) or reuse `InFlight`
    /// (so a stalled commit is indistinguishable from a running population).
    /// Both make `partial_commit_visible = false` impossible to check.
    Prepared,
    /// The last population failed terminally.
    Failed,
}

impl EntryState {
    /// Whether a read may observe a committed value in this state.
    ///
    /// `Prepared` is excluded on purpose. An uncommitted write must read as a
    /// miss, never as a value: that is the whole mechanism behind
    /// `partial_commit_visible = false`.
    #[must_use]
    pub const fn is_readable(self) -> bool {
        matches!(self, Self::Ready | Self::Stale)
    }

    /// Whether a population may claim this state.
    #[must_use]
    pub const fn is_claimable(self) -> bool {
        matches!(self, Self::Absent | Self::Failed | Self::Stale)
    }
}

/// What kind of commit an in-flight intent represents.
///
/// Recorded so recovery knows which direction to resolve it. The two directions
/// are not interchangeable: aborting a prepared *write* is safe because the value
/// is reproducible by re-fetching, whereas aborting a prepared *move* would
/// discard an already-committed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntentKind {
    /// A `set` or a population: prepare -> write -> commit.
    Write,
    /// A `promote` or `demote`: prepare -> read src -> write dst -> remove src -> commit.
    Move,
}

impl IntentKind {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Write => 1,
            Self::Move => 2,
        }
    }

    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Write),
            2 => Some(Self::Move),
            _ => None,
        }
    }
}

/// A recorded, not-yet-committed write intent.
///
/// This is the write-ahead record of the two-phase commit, and it holds **no
/// payload**: key hash, target rung, generation and kind only. The control plane
/// stays payload-free, so a crash leaves an intent that says "a write to this key
/// at this generation was attempted" — enough to resolve it, not enough to serve
/// it. Recovery for a [`IntentKind::Write`] therefore always aborts: the value
/// can be re-fetched, and serving an unverified one would be exactly the silent
/// corruption the contract forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitIntent {
    pub kind: IntentKind,
    pub target_tier: TierIdLite,
    pub generation: Generation,
    /// Nanoseconds since the process clock anchor, for the recovery sweep.
    pub started_nanos: u64,
}

/// The rung an intent targets, stored as its index so the record stays
/// allocation-free and `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TierIdLite(pub u8);

impl TierIdLite {
    #[must_use]
    pub const fn new(index: u8) -> Self {
        Self(index)
    }
}

/// Proof that a caller owns a prepared commit.
///
/// The token carries the generation it was prepared for, so `commit` can verify
/// the entry has not moved underneath it. Presenting a token for a generation
/// that is no longer current is rejected rather than silently applied, which is
/// what stops two racing writers from both believing they committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitToken {
    pub key_hash: u64,
    pub generation: Generation,
    pub kind: IntentKind,
    pub target_tier: TierIdLite,
}

impl CommitToken {
    #[must_use]
    pub const fn expired_by(&self, current: Generation) -> bool {
        current.0 != self.generation.0
    }
}

#[derive(Debug)]
pub struct CacheEntry<V> {
    pub value: V,
    pub generation: Generation,
    pub expiration: Option<Duration>,
}

impl<V> CacheEntry<V> {
    pub fn new(value: V, generation: Generation, expiration: Option<Duration>) -> Self {
        CacheEntry {
            value,
            generation,
            expiration,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_is_not_readable() {
        // The single most important property of the new state: an uncommitted
        // write must never be servable.
        assert!(!EntryState::Prepared.is_readable());
        assert!(EntryState::Ready.is_readable());
    }

    #[test]
    fn prepared_is_not_claimable() {
        // A prepared write owns its entry until it commits or aborts. Letting a
        // population claim it would mean two writers on one key.
        assert!(!EntryState::Prepared.is_claimable());
    }

    #[test]
    fn generation_staleness_is_strict() {
        assert!(Generation::new(1).is_stale(Generation::new(2)));
        assert!(!Generation::new(2).is_stale(Generation::new(2)));
        assert!(!Generation::new(3).is_stale(Generation::new(2)));
    }

    #[test]
    fn a_token_dies_when_the_generation_moves() {
        let token = CommitToken {
            key_hash: 1,
            generation: Generation::new(5),
            kind: IntentKind::Write,
            target_tier: TierIdLite::new(1),
        };
        assert!(!token.expired_by(Generation::new(5)));
        assert!(token.expired_by(Generation::new(6)));
    }
}
