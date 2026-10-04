//! Loom: exhaustive interleavings of the control-plane state machine.
//!
//! # Scope, stated honestly
//!
//! Loom can model `loom::sync::Mutex` and `loom::sync::atomic`. It **cannot**
//! model tokio's `Notify`, so the single-flight wait path is out of reach: a
//! test that claimed to cover the lost-wakeup race under loom would be claiming
//! coverage it does not have. That race is covered instead by a deterministic
//! forced-interleaving test in `tests/await_safety.rs` and
//! `tests/negative/main.rs`.
//!
//! What loom *can* do, and what this file does, is enumerate every interleaving of
//! the acquire/prepare/commit/abort/invalidate state machine against the shard
//! lock. That is where the control plane's correctness lives, and it is exactly
//! the part a randomised concurrency test will usually miss.
//!
//! # Feature gating
//!
//! The models compile only under `--features loom-tests`. A bare `--all-features`
//! run compiles the file and every model skips, printing why. Rustflags would
//! have been the conventional `#[cfg(loom)]` route, but rustflags are part of the
//! sccache cache key — switching them invalidates every dependency build, polars
//! alone included — so a feature is much cheaper for the same result.

#![cfg(feature = "loom-tests")]

use std::sync::Arc;

use loom::sync::Mutex;
use loom::sync::atomic::{AtomicU64, Ordering};
use loom::thread;

/// A miniature of the control plane's entry, using loom's primitives.
///
/// Deliberately not the real `ControlEntry`: that one uses `Arc<tokio::sync::Notify>`
/// and `std::time::Instant`, neither of which loom can schedule or advance. What
/// is modelled is the part loom is good at — the compare-exchange claim protocol
/// and the shard lock — so the model stays faithful to the algorithm rather than
/// to the allocation strategy.
struct ModelEntry {
    state: loom::sync::atomic::AtomicU8,
    generation: AtomicU64,
    owner: loom::sync::atomic::AtomicBool,
    intent: loom::sync::atomic::AtomicBool,
}

/// Whether to actually run the models.
///
/// Loom's primitives panic outside `loom::model`, and a model that explodes on a
/// scheduled run is worse than one that skips: the §16 gates include
/// `--all-features`, which compiles this feature, so without this guard every
/// default CI run would fail on tests nobody asked to run. The skip is loud —
/// each test prints why it did nothing — so a green run cannot be mistaken for
/// coverage.
fn loom_enabled() -> bool {
    std::env::var("THESIX_LOOM").is_ok_and(|v| v == "1")
}

macro_rules! require_loom {
    () => {
        if !loom_enabled() {
            eprintln!(
                "loom models skipped: set THESIX_LOOM=1 (or run `cargo xtask run loom`) \
                 to explore the control-plane interleavings"
            );
            return;
        }
    };
}

const ABSENT: u8 = 0;
const IN_FLIGHT: u8 = 1;
const PREPARED: u8 = 2;
const READY: u8 = 3;

impl ModelEntry {
    fn new() -> Self {
        Self {
            state: loom::sync::atomic::AtomicU8::new(ABSENT),
            generation: AtomicU64::new(0),
            owner: loom::sync::atomic::AtomicBool::new(false),
            intent: loom::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// The shard lock, mirroring `Cachelito`'s per-shard `Mutex`.
///
/// Present because the real control plane takes it around every compound
/// operation. Without it the model explores interleavings the implementation
/// cannot reach — `abort` reads the state, `commit` moves it, `abort` stores —
/// and reports them as bugs in code that is in fact serialised. Modelling the
/// lock is what makes a failure here mean something.
type Shard = Mutex<()>;

/// The claim protocol: exactly one caller may take ownership from a claimable
/// state, and every other caller observes that it lost.
fn claim(shard: &Shard, entry: &ModelEntry, tier: u8) -> bool {
    let _guard = shard.lock().expect("shard lock");
    let won = entry
        .state
        .compare_exchange(ABSENT, IN_FLIGHT, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if won {
        entry.owner.store(true, Ordering::Release);
        entry.generation.fetch_add(1, Ordering::AcqRel);
        let _ = tier;
    }
    won
}

/// Prepare: only from a state the entry is allowed to be in.
fn prepare(shard: &Shard, entry: &ModelEntry) -> Option<u64> {
    let _guard = shard.lock().expect("shard lock");
    let generation = entry.generation.load(Ordering::Acquire);
    let prev =
        entry
            .state
            .compare_exchange(IN_FLIGHT, PREPARED, Ordering::AcqRel, Ordering::Acquire);
    prev.ok().map(|_| {
        entry.intent.store(true, Ordering::Release);
        generation
    })
}

/// Commit: requires the entry to still be prepared and the generation unmoved.
fn commit(shard: &Shard, entry: &ModelEntry, token_generation: u64) -> bool {
    let _guard = shard.lock().expect("shard lock");
    if entry.generation.load(Ordering::Acquire) != token_generation {
        return false;
    }
    if entry
        .state
        .compare_exchange(PREPARED, READY, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    entry.intent.store(false, Ordering::Release);
    entry.owner.store(false, Ordering::Release);
    true
}

/// Abort: restores a claimable state, but never overwrites a commit.
///
/// The guard is the point. `commit` and `abort` can interleave such that the
/// committer's `Prepared -> Ready` CAS lands just before the abort runs; without
/// the state check, the abort then overwrites `Ready` with `Absent` and a
/// successfully committed value becomes invisible. This model found that, and
/// the fix is mirrored in `Cachelito::abort`.
fn abort(shard: &Shard, entry: &ModelEntry) -> bool {
    let _guard = shard.lock().expect("shard lock");
    if entry.state.load(Ordering::Acquire) != PREPARED {
        return false;
    }
    entry.intent.store(false, Ordering::Release);
    entry.owner.store(false, Ordering::Release);
    entry.state.store(ABSENT, Ordering::Release);
    true
}

#[test]
fn exactly_one_caller_claims_the_entry() {
    require_loom!();
    loom::model(|| {
        let entry = Arc::new(ModelEntry::new());
        let shard: Arc<Shard> = Arc::new(Mutex::new(()));
        let winners = Arc::new(AtomicU64::new(0));

        let handles: Vec<_> = (0..4)
            .map(|t| {
                let entry = Arc::clone(&entry);
                let shard = Arc::clone(&shard);
                let winners = Arc::clone(&winners);
                thread::spawn(move || {
                    if claim(&shard, &entry, t as u8) {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }

        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "more than one caller claimed the entry; single-flight is broken"
        );
        assert!(entry.owner.load(Ordering::SeqCst));
    });
}

#[test]
fn a_committed_entry_is_never_reclaimed_as_absent() {
    require_loom!();
    loom::model(|| {
        let entry = Arc::new(ModelEntry::new());
        let shard: Arc<Shard> = Arc::new(Mutex::new(()));
        assert!(claim(&shard, &entry, 0));
        let generation = prepare(&shard, &entry).expect("prepare");
        assert!(commit(&shard, &entry, generation));

        // A second claim must fail: the entry is Ready, not Absent.
        assert!(
            !claim(&shard, &entry, 1),
            "a committed entry was reclaimed as absent"
        );
        assert_eq!(entry.state.load(Ordering::SeqCst), READY);
    });
}

#[test]
fn an_aborted_intent_leaves_the_entry_reclaimable() {
    require_loom!();
    loom::model(|| {
        let entry = Arc::new(ModelEntry::new());
        let shard: Arc<Shard> = Arc::new(Mutex::new(()));
        assert!(claim(&shard, &entry, 0));
        let generation = prepare(&shard, &entry).expect("prepare");
        assert!(
            abort(&shard, &entry),
            "a prepared entry could not be aborted"
        );
        assert!(!entry.intent.load(Ordering::SeqCst));
        // Committed at the old generation, so it must be refused.
        assert!(
            !commit(&shard, &entry, generation),
            "a token committed after an abort"
        );
        assert!(
            claim(&shard, &entry, 1),
            "an aborted entry could not be reclaimed"
        );
    });
}

#[test]
fn a_stale_token_never_commits() {
    require_loom!();
    loom::model(|| {
        let entry = Arc::new(ModelEntry::new());
        let shard: Arc<Shard> = Arc::new(Mutex::new(()));
        assert!(claim(&shard, &entry, 0));
        let stale = prepare(&shard, &entry).expect("prepare");
        // Another writer bumps the generation underneath the first token.
        entry.generation.fetch_add(1, Ordering::AcqRel);
        assert!(!commit(&shard, &entry, stale), "a stale token committed");
    });
}

#[test]
fn concurrent_commit_and_abort_leave_a_settled_state() {
    require_loom!();
    loom::model(|| {
        let entry = Arc::new(ModelEntry::new());
        let shard: Arc<Shard> = Arc::new(Mutex::new(()));
        assert!(claim(&shard, &entry, 0));
        let generation = prepare(&shard, &entry).expect("prepare");

        let e1 = Arc::clone(&entry);
        let e2 = Arc::clone(&entry);
        let s1 = Arc::clone(&shard);
        let s2 = Arc::clone(&shard);
        let committer = thread::spawn(move || commit(&s1, &e1, generation));
        let aborter = thread::spawn(move || abort(&s2, &e2));
        let committed = committer.join().expect("join");
        let aborted = aborter.join().expect("join");

        // Whichever order they ran in, the entry settles and nothing committed is
        // lost. If the commit won, the abort must have declined.
        let state = entry.state.load(Ordering::SeqCst);
        assert!(
            state != PREPARED,
            "the entry was left Prepared; a reader could never settle on it"
        );
        if committed {
            assert_eq!(
                state, READY,
                "a commit that won was overwritten by a concurrent abort"
            );
            assert!(!aborted, "an abort overwrote a commit that had already won");
        }
        if aborted {
            assert!(!committed, "both a commit and an abort reported success");
        }
    });
}

#[test]
fn the_shard_lock_serialises_independent_keys_correctly() {
    require_loom!();
    loom::model(|| {
        // A stand-in for the shard map: one mutex over several entries, mirroring
        // `Cachelito`'s per-shard lock. loom explores the interleavings of threads
        // that each hold it for a compound operation.
        let map = Arc::new(Mutex::new(Vec::<ModelEntry>::new()));
        let handles: Vec<_> = (0..3)
            .map(|i| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    let mut guard = map.lock().expect("lock");
                    while guard.len() <= i {
                        guard.push(ModelEntry::new());
                    }
                    // A compound read-modify-write, exactly the shape that makes
                    // `set_generation(snapshot + 1)` unsafe when done outside the
                    // lock.
                    let g = guard[i].generation.load(Ordering::Acquire);
                    guard[i].generation.store(g + 1, Ordering::Release);
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }

        let guard = map.lock().expect("lock");
        for (i, e) in guard.iter().enumerate() {
            let g = e.generation.load(Ordering::SeqCst);
            assert_eq!(g, 1, "entry {i} holds generation {g}, expected 1");
        }
    });
}

/// The invariant the whole control plane rests on, stated once.
#[test]
fn an_entry_is_never_claimed_while_prepared() {
    require_loom!();
    loom::model(|| {
        let entry = Arc::new(ModelEntry::new());
        let shard: Arc<Shard> = Arc::new(Mutex::new(()));
        assert!(claim(&shard, &entry, 0));
        prepare(&shard, &entry).expect("prepare");

        let e1 = Arc::clone(&entry);
        let e2 = Arc::clone(&entry);
        let sa = Arc::clone(&shard);
        let sb = Arc::clone(&shard);
        let a = thread::spawn(move || claim(&sa, &e1, 1));
        let b = thread::spawn(move || claim(&sb, &e2, 2));
        let first = a.join().expect("join");
        let second = b.join().expect("join");

        assert!(
            !(first && second),
            "two callers claimed a prepared entry; a partial write could be observed"
        );
        assert!(!first && !second, "a prepared entry was claimable at all");
    });
}
