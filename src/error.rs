use thiserror::Error;

#[derive(Debug, Error)]
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
}
