//! The spawned governor task and its handle.
//!
//! The loop is deliberately trivial — sleep, tick, repeat — because everything
//! that could go wrong has already been made safe at the layer below: each tick
//! performs bounded work through the manager, and every manager operation is
//! cancel-safe via the `PopulationGuard` / `IntentGuard` drops. Aborting the
//! task mid-tick therefore leaves exactly the same recoverable state as
//! cancelling any consumer operation: at worst an intent for the next sweep to
//! resolve, never a wedged key.

use std::sync::Arc;
use std::time::Duration;

use crate::error::CacheError;
use crate::governor::Governor;
use crate::policy::CachePolicy;

/// Cancellation and completion handle for the spawned governor.
///
/// Intentionally narrower than a raw `JoinHandle`: the governor loop never
/// returns a value, so there is nothing to join — what a caller needs is the
/// ability to stop it and to know it exited cleanly when the runtime shuts down.
#[derive(Debug)]
pub struct GovernorHandle {
    abort: tokio::task::AbortHandle,
    join: tokio::task::JoinHandle<()>,
    interval: Duration,
}

impl GovernorHandle {
    /// Stop the task at its next opportunity.
    ///
    /// Idempotent: an already-exited task accepts the abort silently. A tick in
    /// flight completes its current bounded action or unwinds through the
    /// manager's guards, whichever comes first.
    pub fn abort(&self) {
        self.abort.abort();
    }

    /// The configured interval, for diagnostics.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// Wait for the task to exit, however it exits.
    ///
    /// Nothing useful to report from a cancelled or panicked loop: the panic
    /// surface is the manager's own, and the runtime logs it with the task.
    pub async fn join(self) {
        let _ = self.join.await;
    }
}

/// Spawn the governor loop for a [`FullyAutonomous`](crate::governor::GovernorMode)
/// governor.
///
/// # Errors
///
/// [`CacheError::ConfigurationError`] when the governor is not configured for
/// `FullyAutonomous`: spawning a loop that would do nothing is a configuration
/// mistake worth refusing, and the caller always owns a `Manual`/`Semi`
/// governor's schedule anyway.
pub fn spawn<K, V, P>(governor: Arc<Governor<K, V, P>>) -> Result<GovernorHandle, CacheError>
where
    K: crate::key::Key + 'static,
    V: Clone + Send + Sync + 'static,
    P: CachePolicy<K, V> + 'static,
{
    if governor.settings().mode != crate::governor::GovernorMode::FullyAutonomous {
        return Err(CacheError::ConfigurationError);
    }
    let interval = governor.settings().interval.max(Duration::from_millis(1));
    let handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if let Err(err) = governor.tick().await {
                // The loop does not die on one poisoned shard: the next tick
                // re-observes, and the same fault will be visible in
                // `continuity()` and `plane_stats` either way.
                let _ = err;
            }
        }
    });
    Ok(GovernorHandle {
        abort: handle.abort_handle(),
        join: handle,
        interval,
    })
}
