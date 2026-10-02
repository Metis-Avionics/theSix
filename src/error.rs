use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CacheError {
    #[error("unauthenticated")]
    Unauthenticated,

    #[error("unauthorized")]
    Unauthorized,

    #[error("tier unavailable")]
    TierUnavailable,

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
}
