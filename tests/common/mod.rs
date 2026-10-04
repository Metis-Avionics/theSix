//! Backwards-compatible alias for the shared harness.
//!
//! The helpers moved to the `testkit` crate so that every test target —
//! including the ones in subdirectories, which cannot reach `tests/common/` by
//! module path — uses one harness. This file stays as a re-export so the
//! existing flat test targets keep compiling unchanged.
//!
//! It is deliberately not duplicated: two copies of `make_manager` would drift,
//! and a drifted harness makes every failure ambiguous.
// Each test target pulls in only the helpers it uses, so most re-exports are
// unused in any given file. `unused_imports` is allowed for that reason and no
// other.
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(clippy::wildcard_imports)]

pub use testkit::{
    FaultyTier, HangingTier, RecordingTier, Tally, anon_ctx, corrupt_string, ctx_for_tenant,
    default_tiers, faulty, faulty_corrupting, ladder_with, make_manager, make_manager_with_timeout,
    manager_from_tiers, record_all, test_ctx,
};

pub mod coverage {
    pub use testkit::coverage::*;
}
