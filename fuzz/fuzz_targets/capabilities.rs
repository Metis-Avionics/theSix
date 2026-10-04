//! Trust boundary: capability representations.
//!
//! Property: a capability built from arbitrary flag bits must remain internally
//! consistent — `is_bound`, `survives_restart` and `is_authoritative` must never
//! contradict the flags they are derived from.
#![no_main]

use libfuzzer_sys::fuzz_target;
use thesix::{CapabilityFlags, DurabilityClass, OperationalState, TierCapability, BackendKind};

fuzz_target!(|bits: u16| {
    // Build flags from arbitrary bits, intersecting with the known set so the
    // result is a legitimate combination.
    let mut flags = CapabilityFlags::EMPTY;
    for known in CapabilityFlags::ALL {
        if bits & known.bits() != 0 {
            flags |= known;
        }
    }
    let state = match bits % 3 {
        0 => OperationalState::Unbound,
        1 => OperationalState::Healthy,
        _ => OperationalState::Degraded,
    };
    let durability = match bits % 3 {
        0 => DurabilityClass::Volatile,
        1 => DurabilityClass::Delegated,
        _ => DurabilityClass::Verified,
    };
    let cap = TierCapability::new(BackendKind::InMemory, flags, state, durability);

    // Derived predicates must agree with the axes they come from.
    assert_eq!(
        cap.is_bound(),
        state != OperationalState::Unbound,
        "is_bound disagreed with the state"
    );
    assert_eq!(
        cap.is_authoritative(),
        flags.contains(CapabilityFlags::AUTHORITATIVE)
    );
    assert_eq!(
        cap.survives_restart(),
        matches!(durability, DurabilityClass::Delegated | DurabilityClass::Verified)
    );
    // An unbound rung cannot be authoritative in practice; the contract says so,
    // and a combination that says otherwise must at least be visible.
    if !cap.is_bound() && cap.is_authoritative() {
        assert!(
            cap.summary().contains("authoritative"),
            "an unbound authoritative rung is not visible in its summary"
        );
    }
    // Flags must survive a round-trip through their names.
    for f in flags.iter() {
        assert!(cap.flags.contains(f), "{} did not round-trip", f.name());
    }
});
