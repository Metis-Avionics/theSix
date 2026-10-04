use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CacheError {
    #[error("unauthenticated")]
    Unauthenticated,

    #[error("unauthorized")]
    Unauthorized,

    #[error("tier unavailable")]
    TierUnavailable,

    /// A write whose outcome is unknown: the backend may have stored the bytes
    /// and may not have, and it cannot say which.
    ///
    /// This is a distinct condition from every other error here, because it is the
    /// only one where the caller's next action depends on the *data plane* rather
    /// than on the control plane. A definite failure — a full tier, a rejected
    /// write — provably stored nothing, so there is nothing to clean up and
    /// deleting the key would destroy the value already there. An indeterminate
    /// failure may have overwritten a good value with one nobody authorised, so
    /// the key has to go before the intent is released.
    ///
    /// Conflating the two is what made a failed write either leak a value or
    /// destroy a good one, depending on which mistake was made.
    #[error("write outcome indeterminate")]
    WriteIndeterminate,

    #[error("policy denied")]
    PolicyDenied,

    #[error("cache miss")]
    Miss,

    #[error("population failed")]
    PopulationFailed,

    #[error("timeout")]
    Timeout,

    #[error("cancelled")]
    Cancelled,

    #[error("stale generation")]
    StaleGeneration,

    #[error("serialization failed")]
    SerializationFailed,

    #[error("configuration error")]
    ConfigurationError,
    /// A fixed-capacity tier is full.
    ///
    /// Was reported as `ConfigurationError`, which is a category error: a full
    /// table is a runtime condition reached under load, not a misconfiguration,
    /// and a caller reacting to it as the latter will treat a busy cache as a
    /// fatal deployment fault.
    #[error("tier capacity exhausted")]
    CapacityExhausted,

    /// Stored bytes did not match their integrity digest, or a backend returned
    /// a record too short to carry one.
    ///
    /// Distinct from `SerializationFailed` on purpose: that variant means "I
    /// could not decode this", while this means "this is not the record I
    /// wrote". Collapsing them is how silent corruption becomes a decode
    /// error the caller retries past, and the caller retries because it
    /// believes the next read will succeed.
    #[error("integrity check failed")]
    Corrupted,

    /// An in-flight commit intent exists for this key: a write reached the
    /// `prepare` phase and did not reach `commit` or `abort`.
    ///
    /// Surfacing this rather than reading as `Miss` is what makes
    /// `partial_commit_visible = false` checkable. A read of a `Prepared` entry
    /// is a miss, but recovery-visible diagnostics are not, so the manager can
    /// tell "nothing is stored" apart from "something was started".
    #[error("uncommitted write intent present")]
    UncommittedIntent,
}
