//! Capability semantics.
//!
//! "Which backend is this, and can I rely on it?" previously had no honest
//! answer. A consumer could only find out by issuing an operation and receiving
//! `TierUnavailable`, which cannot distinguish *not compiled in*, *bound but
//! down*, and *never implemented* — three situations that demand different
//! responses: change the build, retry, or stop asking.
//!
//! # Why two axes and not one enum
//!
//! The obvious design is a single `enum Capability { Unbound, Unavailable,
//! InMemory, Persistent, ... }`. It cannot work, because these properties are
//! not mutually exclusive: a Postgres authority rung is persistent *and* shared
//! *and* authoritative *and* possibly degraded right now. An enum forces a
//! choice, so every combination gets its own variant and the list becomes
//! unrepresentable exactly where it matters.
//!
//! So the vocabulary is split the way the contract splits it:
//!
//! * [`CapabilityFlags`] — durable properties of the binding. Orthogonal,
//!   composable, cheap to test.
//! * [`OperationalState`] — what is happening right now. Mutually exclusive,
//!   because a rung is in one state at a time.
//! * [`DurabilityClass`] — how much crash-safety is claimed, and by what
//!   evidence. Kept separate from `PERSISTENT` on purpose: a store can persist
//!   across a `set` while nothing in this crate has ever verified it survives a
//!   process restart.

use std::fmt;

use crate::tier::tier_trait::BackendKind;

/// A durable property of a rung's binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityFlags(u16);

impl CapabilityFlags {
    /// No properties claimed.
    pub const EMPTY: CapabilityFlags = CapabilityFlags(0);
    /// Values live in this process and nowhere else.
    pub const IN_MEMORY: CapabilityFlags = CapabilityFlags(1 << 0);
    /// Values do not survive a process restart.
    pub const VOLATILE: CapabilityFlags = CapabilityFlags(1 << 1);
    /// Values are written to something that outlives the process.
    pub const PERSISTENT: CapabilityFlags = CapabilityFlags(1 << 2);
    /// The store is reachable from more than one process.
    pub const SHARED: CapabilityFlags = CapabilityFlags(1 << 3);
    /// This rung is the system of record for its keys.
    pub const AUTHORITATIVE: CapabilityFlags = CapabilityFlags(1 << 4);
    /// Operations block the calling thread. A consumer that cares about reactor
    /// threads needs to know this before it awaits one.
    pub const BLOCKING_IO: CapabilityFlags = CapabilityFlags(1 << 5);

    /// Every flag, for iteration and round-tripping.
    pub const ALL: [CapabilityFlags; 6] = [
        Self::IN_MEMORY,
        Self::VOLATILE,
        Self::PERSISTENT,
        Self::SHARED,
        Self::AUTHORITATIVE,
        Self::BLOCKING_IO,
    ];

    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        CapabilityFlags(self.0 | other.0)
    }

    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        CapabilityFlags(self.0 & !other.0)
    }

    /// The individual flags set, for reporting.
    pub fn iter(self) -> impl Iterator<Item = CapabilityFlags> {
        Self::ALL.into_iter().filter(move |f| self.contains(*f))
    }

    /// The contract spellings of the flags that are set.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        self.iter().map(Self::name).collect()
    }

    /// The contract spelling of a single flag.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::IN_MEMORY => "in_memory",
            Self::VOLATILE => "volatile",
            Self::PERSISTENT => "persistent",
            Self::SHARED => "shared",
            Self::AUTHORITATIVE => "authoritative",
            Self::BLOCKING_IO => "blocking_io",
            _ => "unknown",
        }
    }
}

impl std::ops::BitOr for CapabilityFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for CapabilityFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Display for CapabilityFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self.names();
        if names.is_empty() {
            f.write_str("none")
        } else {
            f.write_str(&names.join("|"))
        }
    }
}

/// What a rung is doing right now.
///
/// Unlike [`CapabilityFlags`], these are mutually exclusive: a rung is in one
/// state at a time, and collapsing them would reintroduce exactly the
/// conflation the capability surface was added to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationalState {
    /// No implementation is registered for this rung.
    Unbound,
    /// Registered, but refusing operations right now.
    Unavailable,
    /// Serving normally.
    Healthy,
    /// Serving, with failures recorded and a fallback in use.
    Degraded,
    /// Coming back after a failure; not yet trusted.
    Recovering,
    /// Comparing its contents against the authority and repairing drift.
    Reconciling,
}

impl OperationalState {
    pub const ALL: [OperationalState; 6] = [
        Self::Unbound,
        Self::Unavailable,
        Self::Healthy,
        Self::Degraded,
        Self::Recovering,
        Self::Reconciling,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Unbound => "unbound",
            Self::Unavailable => "unavailable",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Recovering => "recovering",
            Self::Reconciling => "reconciling",
        }
    }

    /// Whether operations are expected to succeed right now.
    #[must_use]
    pub fn is_serving(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }
}

impl fmt::Display for OperationalState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How much crash-safety is claimed for a rung's values, and on what evidence.
///
/// This is the mechanism behind `no_false_durability`. `PERSISTENT` in
/// [`CapabilityFlags`] says the store is not memory-only; `DurabilityClass`
/// says whether anything has actually *proved* the values come back. A rung
/// whose backend would survive a restart, but which this crate has never tested
/// end to end, is `Delegated` — honest, and upgradeable to `Verified` the moment
/// a test drops the store and reopens it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DurabilityClass {
    /// Gone when the process exits. Nothing is claimed.
    Volatile,
    /// The backend's own defaults are trusted without a test in this crate.
    Delegated,
    /// A test in this crate drops the store and reads the value back.
    Verified,
}

impl DurabilityClass {
    pub const ALL: [DurabilityClass; 3] = [Self::Volatile, Self::Delegated, Self::Verified];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Volatile => "volatile",
            Self::Delegated => "delegated",
            Self::Verified => "verified",
        }
    }

    /// Whether values are claimed to survive a process restart at all.
    #[must_use]
    pub fn survives_restart(self) -> bool {
        matches!(self, Self::Delegated | Self::Verified)
    }
}

impl fmt::Display for DurabilityClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Everything a consumer can learn about a rung without touching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierCapability {
    /// What the rung is bound to. Cannot claim a backend it was not built with:
    /// it is returned by the tier instance itself.
    pub backend: BackendKind,
    /// Durable properties of the binding.
    pub flags: CapabilityFlags,
    /// Current condition.
    pub state: OperationalState,
    /// Crash-safety claim and its evidence.
    pub durability: DurabilityClass,
}

impl TierCapability {
    /// A rung with nothing registered.
    #[must_use]
    pub const fn unbound() -> Self {
        Self {
            backend: BackendKind::Unavailable,
            flags: CapabilityFlags::EMPTY,
            state: OperationalState::Unbound,
            durability: DurabilityClass::Volatile,
        }
    }

    #[must_use]
    pub const fn new(
        backend: BackendKind,
        flags: CapabilityFlags,
        state: OperationalState,
        durability: DurabilityClass,
    ) -> Self {
        Self {
            backend,
            flags,
            state,
            durability,
        }
    }

    /// Whether an implementation is registered for this rung.
    ///
    /// This replaces `BackendKind::is_native()`, which answered `true` for
    /// `Postgres`, `RocksDb`, `Neo4j`, `Helix` and `Moka` — none of which exist in this
    /// crate. Reporting a backend that was never implemented is precisely the
    /// misreporting the contract forbids, and it was the default answer.
    #[must_use]
    pub const fn is_bound(&self) -> bool {
        !matches!(self.state, OperationalState::Unbound)
    }

    /// Whether this rung is the system of record.
    #[must_use]
    pub const fn is_authoritative(&self) -> bool {
        self.flags.contains(CapabilityFlags::AUTHORITATIVE)
    }

    /// Whether operations block the calling thread.
    #[must_use]
    pub const fn is_blocking_io(&self) -> bool {
        self.flags.contains(CapabilityFlags::BLOCKING_IO)
    }

    /// Whether a consumer may rely on values outliving the process.
    #[must_use]
    pub const fn survives_restart(&self) -> bool {
        matches!(
            self.durability,
            DurabilityClass::Delegated | DurabilityClass::Verified
        )
    }

    /// One line suitable for a log or a failure message. Contains no payload and
    /// no key material.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{}[{}] state={} durability={}",
            self.backend, self.flags, self.state, self.durability
        )
    }
}

/// Every capability name the crate can report, across all three axes.
///
/// The contract's `[capabilities.states].allowed` list is a consumer-facing
/// vocabulary that deliberately mixes the axes — a caller asks "is it
/// persistent?", not "which axis is persistent on?". This function is the
/// implementation-side answer to "can we say that?", and the contract gate
/// asserts the two agree.
#[must_use]
pub fn all_state_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    names.extend(CapabilityFlags::ALL.iter().map(|f| f.name()));
    names.extend(OperationalState::ALL.iter().map(|s| s.name()));
    names.extend(DurabilityClass::ALL.iter().map(|d| d.name()));
    names.sort_unstable();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_compose() {
        let c = CapabilityFlags::IN_MEMORY | CapabilityFlags::VOLATILE;
        assert!(c.contains(CapabilityFlags::IN_MEMORY));
        assert!(c.contains(CapabilityFlags::VOLATILE));
        assert!(!c.contains(CapabilityFlags::PERSISTENT));
        // A rung can be persistent *and* shared *and* authoritative at once,
        // which is exactly what a single enum could not express.
        let authority =
            CapabilityFlags::PERSISTENT | CapabilityFlags::SHARED | CapabilityFlags::AUTHORITATIVE;
        assert!(authority.contains(CapabilityFlags::PERSISTENT));
        assert!(authority.contains(CapabilityFlags::AUTHORITATIVE));
    }

    #[test]
    fn without_removes_only_the_named_flag() {
        let c = CapabilityFlags::IN_MEMORY | CapabilityFlags::VOLATILE;
        let c = c.without(CapabilityFlags::VOLATILE);
        assert!(c.contains(CapabilityFlags::IN_MEMORY));
        assert!(!c.contains(CapabilityFlags::VOLATILE));
    }

    #[test]
    fn every_contract_capability_name_is_representable() {
        let have = all_state_names();
        for required in [
            "unbound",
            "unavailable",
            "volatile",
            "persistent",
            "authoritative",
            "shared",
            "degraded",
            "recovering",
            "in_memory",
        ] {
            assert!(
                have.contains(&required),
                "{required:?} is required by the contract but no axis can report it"
            );
        }
    }

    #[test]
    fn unbound_is_distinct_from_unavailable() {
        let unbound = TierCapability::unbound();
        let unavailable = TierCapability::new(
            BackendKind::InMemory,
            CapabilityFlags::IN_MEMORY,
            OperationalState::Unavailable,
            DurabilityClass::Volatile,
        );
        assert!(!unbound.is_bound());
        assert!(unavailable.is_bound());
        assert_ne!(unbound.state, unavailable.state);
    }

    #[test]
    fn delegating_durability_is_not_claiming_verified() {
        let d = TierCapability::new(
            BackendKind::Sled,
            CapabilityFlags::PERSISTENT,
            OperationalState::Healthy,
            DurabilityClass::Delegated,
        );
        assert!(d.survives_restart());
        assert_ne!(d.durability, DurabilityClass::Verified);
    }

    #[test]
    fn capability_summary_carries_no_key_material() {
        let c = TierCapability::new(
            BackendKind::Sled,
            CapabilityFlags::PERSISTENT,
            OperationalState::Healthy,
            DurabilityClass::Verified,
        );
        let s = c.summary();
        assert!(s.contains("sled"));
        assert!(s.contains("persistent"));
        assert!(s.contains("verified"));
    }
}
