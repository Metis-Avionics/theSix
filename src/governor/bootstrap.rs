//! Ladder assembly from the environment.
//!
//! The six builds the governor manages are assembled here: detect what the
//! environment offers, bind what can be bound, degrade what cannot — loudly —
//! and hand back the pieces autonomy needs (the ladder, the rebindable rungs, a
//! rebinder that can rebuild a failed rung).
//!
//! # What detection is allowed to assume
//!
//! Redis and Valkey are the same wire protocol (RESP), so one backend serves
//! both. A `valkey://` or `valkeys://` URL is normalised to its `redis://` /
//! `rediss://` spelling before the client sees it, because the `redis` crate at
//! this version parses only the latter — and because scheme preference is a
//! configuration spelling, not a different service.
//!
//! # What detection is allowed to degrade
//!
//! A URL that is **absent** binds nothing redis-shaped: L3 falls back to its
//! in-memory stub and the report says so. A URL that is **present but
//! unreachable** degrades the same way, with the reason named in the report, so
//! the governor can rebind L3 to a real backend later when the server appears —
//! that is the autonomous path, and refusing to start would defeat it. A URL
//! that is **malformed** is refused outright: a configuration that can never
//! connect must fail at build time, not limp along with a report nobody reads.
//! All three outcomes are loud; none is a silent substitution.

use std::sync::Arc;

use crate::error::CacheError;
use crate::tier::hot_rung::HotRungTier;
use crate::tier::tier_trait::CacheTier;

/// Environment variable holding a redis or valkey URL, theSix-scoped spelling.
pub const ENV_THESIX_REDIS_URL: &str = "THESIX_REDIS_URL";
/// Environment variable holding a redis or valkey URL, theSix-scoped spelling.
pub const ENV_THESIX_VALKEY_URL: &str = "THESIX_VALKEY_URL";
/// Environment variable holding a redis or valkey URL, the well-known spelling.
pub const ENV_REDIS_URL: &str = "REDIS_URL";
/// Environment variable holding a redis or valkey URL, the well-known spelling.
pub const ENV_VALKEY_URL: &str = "VALKEY_URL";

/// A source of configuration strings.
///
/// Injectable so detection is testable without mutating process state: the 2024
/// edition makes `std::env::set_var` unsafe, and this crate is
/// `#![forbid(unsafe_code)]` — so the tests cannot set environment variables,
/// and a process-env singleton would make the detection untestable. Both of
/// those facts point at the same design, so the trait is not ceremony.
pub trait EnvSource: Send + Sync {
    /// The value of `name`, or `None` when unset. An empty value counts as
    /// unset: an empty URL is a misspelling waiting to happen, and
    /// `REDIS_URL=` silently selecting nothing would be indistinguishable from
    /// not configuring redis at all.
    fn lookup(&self, name: &str) -> Option<String>;
}

/// The process environment, after loading a `.env` file when the `dotenv`
/// feature is compiled in.
///
/// Loading happens at construction, once: a later process-env mutation is
/// invisible to this source, which keeps "what did the config say" a fact about
/// startup rather than a moving target.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl ProcessEnv {
    /// Load `.env` into the process environment (no-op without the `dotenv`
    /// feature), then read configuration from `std::env`.
    #[must_use]
    pub fn load_dotenv() -> Self {
        #[cfg(feature = "dotenv")]
        {
            // A missing file is the normal case for a process configured by its
            // orchestrator; a malformed one is allowed to surface through the
            // variables that do load, which is dotenvy's own contract.
            let _ = dotenvy::dotenv();
        }
        ProcessEnv
    }
}

impl EnvSource for ProcessEnv {
    fn lookup(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }
}

/// An explicit mapping, for tests and embedders. Lookup order is insertion
/// order; the last value for a name wins, as a map would.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(std::collections::BTreeMap<String, String>);

impl MapEnv {
    #[must_use]
    pub fn new() -> Self {
        MapEnv(std::collections::BTreeMap::new())
    }

    #[must_use]
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.0.insert(name.to_string(), value.to_string());
        self
    }
}

impl EnvSource for MapEnv {
    fn lookup(&self, name: &str) -> Option<String> {
        self.0
            .get(name)
            .cloned()
            .filter(|value| !value.trim().is_empty())
    }
}

/// A redis-shaped endpoint the environment offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedUrl {
    /// The URL after scheme normalisation, ready for `L3RedisBackend::connect`.
    pub normalized: String,
    /// Whether the environment named it as valkey (either by `valkey://` scheme
    /// or by the variable name). Reported so an operator can tell which service
    /// they configured; the protocol is the same either way.
    pub valkey: bool,
}

/// Read the environment once, in declared preference order.
///
/// Preference: theSix-scoped spellings first (`THESIX_*`), then the well-known
/// ones. Within each spelling redis before valkey is irrelevant — a variable
/// names its scheme honestly — but the ordering here is what makes
/// `THESIX_VALKEY_URL` beat a stale `REDIS_URL`, which is the whole point of a
/// scoped spelling.
///
/// # Errors
///
/// [`CacheError::ConfigurationError`] when the highest-preference variable that
/// is present does not carry a redis-shaped URL. Present-but-invalid refuses;
/// it does not fall through to a lower-preference variable, because "half the
/// configuration was ignored" is exactly the silent behaviour these reports
/// exist to prevent.
///
/// Returns `Ok(None)` when no variable is set: a rung with no endpoint binds no
/// redis backend, and that is configuration, not an error.
pub fn detect_redis_endpoint(env: &dyn EnvSource) -> Result<Option<DetectedUrl>, CacheError> {
    let candidates: [(&str, bool); 4] = [
        (ENV_THESIX_REDIS_URL, false),
        (ENV_THESIX_VALKEY_URL, true),
        (ENV_REDIS_URL, false),
        (ENV_VALKEY_URL, true),
    ];
    for (name, valkey_by_name) in candidates {
        if let Some(raw) = env.lookup(name) {
            return normalize_url(&raw, valkey_by_name).map(Some);
        }
    }
    Ok(None)
}

/// Normalise one URL's scheme into what the `redis` crate parses.
fn normalize_url(raw: &str, valkey_by_name: bool) -> Result<DetectedUrl, CacheError> {
    let (normalized, valkey_by_scheme) = if let Some(rest) = raw.strip_prefix("valkey://") {
        (format!("redis://{rest}"), true)
    } else if let Some(rest) = raw.strip_prefix("valkeys://") {
        (format!("rediss://{rest}"), true)
    } else if raw.starts_with("redis://") || raw.starts_with("rediss://") {
        (raw.to_string(), false)
    } else {
        // Payload-free by the same rule as every other error in this crate: the
        // URL is configuration material and may carry credentials, so neither
        // the error nor its message repeats it.
        return Err(CacheError::ConfigurationError);
    };
    Ok(DetectedUrl {
        normalized,
        valkey: valkey_by_scheme || valkey_by_name,
    })
}

/// How L3 turned out, named for what an operator would ask next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L3Binding {
    /// No redis-shaped URL was configured. L3 is the in-memory stub, and no
    /// rebinder was produced: there is nothing to reconnect to.
    NotConfigured,
    /// A real backend connected. The URL is the normalised one.
    Redis { url: String, valkey: bool },
    /// A URL was configured but redis is not serving this process: either the
    /// feature is not compiled in or the server refused the connection. L3 runs
    /// its stub, and — with the feature present — a rebinder exists so
    /// autonomy can put the real backend in when it comes back.
    RedisDegradedToMemory {
        url: String,
        valkey: bool,
        reason: String,
    },
}

/// What one ladder assembly produced.
pub struct Ladder<V> {
    /// The six cache rungs, in ladder order, ready for `CacheManager::new`.
    /// The authority rung is deliberately absent: wiring authority is a
    /// configured role (`with_authority` + an origin tier), not environment
    /// detection, and nothing here may guess at it.
    pub tiers: Vec<Arc<dyn CacheTier<V>>>,
    /// The subset of `tiers` that autonomy may rebind at runtime.
    pub hot_rungs: Vec<Arc<HotRungTier<V>>>,
    /// When present, autonomy may rebuild the failed rungs this names.
    pub rebinder: Option<crate::governor::Rebinder<V>>,
    pub report: BindingReport,
}

/// The binding outcome, per rung.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BindingReport {
    pub l3: Option<L3Binding>,
}

/// Assemble the six cache rungs from the environment.
///
/// # Errors
///
/// [`CacheError::ConfigurationError`] for a malformed URL, per
/// [`detect_redis_endpoint`]. Connection failures are *not* errors: they are
/// degradations named in the report, because the autonomous governor exists to
/// rebind a rung whose backend is down.
pub fn assemble_ladder<V>(env: &dyn EnvSource) -> Result<Ladder<V>, CacheError>
where
    V: crate::tier::backends::ByteValue + std::hash::Hash,
{
    let endpoint = detect_redis_endpoint(env)?;
    let tiers: Vec<Arc<dyn CacheTier<V>>> = vec![
        Arc::new(crate::tier::l0::L0Stub::new()),
        Arc::new(crate::tier::l1::L1Stub::new()),
        Arc::new(crate::tier::l2::L2Stub::new()),
        Arc::new(crate::tier::l3::L3Stub::new()),
        Arc::new(crate::tier::l4::L4Stub::new()),
        Arc::new(crate::tier::l5::L5Stub::new()),
    ];
    // The L3 binding is the one detection can change, so it is computed by the
    // feature-switched helper and everything else stays fixed. Keeping the two
    // build shapes in separate functions (rather than `cfg` inside this one) is
    // what keeps the no-redis build free of mutable locals it never mutates.
    let (tiers, hot_rungs, rebinder, l3_binding) = match endpoint {
        None => (tiers, Vec::new(), None, L3Binding::NotConfigured),
        Some(ref detected) => {
            let binding = bind_l3::<V>(detected, tiers)?;
            let tiers = binding.tiers;
            (tiers, binding.hot_rungs, binding.rebinder, binding.binding)
        }
    };

    Ok(Ladder {
        tiers,
        hot_rungs,
        rebinder,
        report: BindingReport {
            l3: Some(l3_binding),
        },
    })
}

/// What binding L3 produced, as a bundle so each build shape computes it whole.
struct L3BindingOutcome<V> {
    tiers: Vec<Arc<dyn CacheTier<V>>>,
    hot_rungs: Vec<Arc<HotRungTier<V>>>,
    rebinder: Option<crate::governor::Rebinder<V>>,
    binding: L3Binding,
}

#[cfg(feature = "redis")]
fn bind_l3<V>(
    detected: &DetectedUrl,
    tiers: Vec<Arc<dyn CacheTier<V>>>,
) -> Result<L3BindingOutcome<V>, CacheError>
where
    V: crate::tier::backends::ByteValue + std::hash::Hash,
{
    let mut tiers = tiers;
    let mut hot_rungs = Vec::new();
    if let Ok(backend) = crate::tier::backends::L3RedisBackend::<V>::connect(&detected.normalized) {
        let hot = Arc::new(HotRungTier::new(
            crate::tier::TierId::L3,
            Arc::new(backend) as Arc<dyn CacheTier<V>>,
        )?);
        tiers[3] = Arc::clone(&hot) as Arc<dyn CacheTier<V>>;
        hot_rungs.push(hot);
        Ok(L3BindingOutcome {
            tiers,
            hot_rungs,
            rebinder: Some(redis_rebinder(detected.clone())),
            binding: L3Binding::Redis {
                url: detected.normalized.clone(),
                valkey: detected.valkey,
            },
        })
    } else {
        let hot = Arc::new(HotRungTier::new(
            crate::tier::TierId::L3,
            Arc::clone(&tiers[3]) as Arc<dyn CacheTier<V>>,
        )?);
        hot_rungs.push(hot);
        Ok(L3BindingOutcome {
            tiers,
            hot_rungs,
            rebinder: Some(redis_rebinder(detected.clone())),
            binding: L3Binding::RedisDegradedToMemory {
                url: detected.normalized.clone(),
                valkey: detected.valkey,
                // Deliberately not the backend error: a connection error can
                // carry a URL and addresses, and this report may end up in
                // logs. The operator has the URL next to the reason by
                // construction.
                reason: "the redis server did not accept a connection".to_string(),
            },
        })
    }
}

#[cfg(not(feature = "redis"))]
fn bind_l3<V>(
    detected: &DetectedUrl,
    tiers: Vec<Arc<dyn CacheTier<V>>>,
) -> Result<L3BindingOutcome<V>, CacheError> {
    // No backend code exists in this build to connect with, so there is nothing
    // to rebind to later either. Both parts of that are the same decision, so
    // the report names it once.
    Ok(L3BindingOutcome {
        tiers,
        hot_rungs: Vec::new(),
        rebinder: None,
        binding: L3Binding::RedisDegradedToMemory {
            url: detected.normalized.clone(),
            valkey: detected.valkey,
            reason: "the redis feature is not compiled in".to_string(),
        },
    })
}

/// A rebinder that reconnects L3 to the endpoint it was assembled with.
///
/// The endpoint is captured at assembly time. A rebinder that re-read the
/// environment on every attempt would race its own operator's edits and could
/// swap the rung to a backend the report has never described, so it is closed
/// over instead — rebinding is infrastructure repair, not configuration reload.
#[cfg(feature = "redis")]
fn redis_rebinder<V>(endpoint: DetectedUrl) -> crate::governor::Rebinder<V>
where
    V: crate::tier::backends::ByteValue + std::hash::Hash,
{
    Arc::new(move |rung| {
        if rung != crate::tier::TierId::L3 {
            return Ok(None);
        }
        match crate::tier::backends::L3RedisBackend::<V>::connect(&endpoint.normalized) {
            Ok(backend) => Ok(Some(Arc::new(backend) as Arc<dyn CacheTier<V>>)),
            Err(e) => Err(e),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> MapEnv {
        MapEnv::new()
    }

    #[test]
    fn a_valkey_url_normalizes_to_what_the_client_parses() {
        testkit::proves!("autonomy.bootstrap.valkey_scheme_is_normalized");

        let env = empty().with(ENV_THESIX_VALKEY_URL, "valkey://valkey.local:6379/0");
        let detected = detect_redis_endpoint(&env)
            .expect("detect")
            .expect("present");
        assert_eq!(detected.normalized, "redis://valkey.local:6379/0");
        assert!(detected.valkey);

        let tls = empty().with(ENV_VALKEY_URL, "valkeys://valkey.local:6380");
        let detected = detect_redis_endpoint(&tls)
            .expect("detect")
            .expect("present");
        assert_eq!(detected.normalized, "rediss://valkey.local:6380");
        assert!(detected.valkey);
    }

    #[test]
    fn a_redis_url_passes_through_unmarked() {
        let env = empty().with(ENV_REDIS_URL, "redis://127.0.0.1:6379");
        let detected = detect_redis_endpoint(&env)
            .expect("detect")
            .expect("present");
        assert_eq!(detected.normalized, "redis://127.0.0.1:6379");
        assert!(!detected.valkey);
    }

    #[test]
    fn the_scoped_spelling_beats_the_well_known_one() {
        let env = empty()
            .with(ENV_REDIS_URL, "redis://stale.example:6379")
            .with(ENV_THESIX_VALKEY_URL, "valkey://fresh.example:6379");
        let detected = detect_redis_endpoint(&env)
            .expect("detect")
            .expect("present");
        assert_eq!(
            detected.normalized, "redis://fresh.example:6379",
            "the six-scoped variable outranks the well-known one"
        );
        assert!(detected.valkey);
    }

    #[test]
    fn a_missing_url_binds_nothing_and_is_not_an_error() {
        testkit::proves!("autonomy.bootstrap.absent_url_binds_no_redis_backend");

        let detected = detect_redis_endpoint(&empty()).expect("detect");
        assert_eq!(detected, None);
    }

    #[test]
    fn an_empty_value_counts_as_absent() {
        let env = empty().with(ENV_REDIS_URL, "   ");
        let detected = detect_redis_endpoint(&env).expect("detect");
        assert_eq!(detected, None, "an empty URL must not reach the parser");
    }

    #[test]
    fn a_non_redis_url_is_a_configuration_error() {
        testkit::proves!("autonomy.bootstrap.invalid_url_is_a_configuration_error");

        let env = empty().with(ENV_REDIS_URL, "postgres://127.0.0.1/db");
        assert_eq!(
            detect_redis_endpoint(&env).unwrap_err(),
            CacheError::ConfigurationError
        );
    }

    #[test]
    fn an_absent_endpoint_reports_not_configured_with_no_rebinder() {
        testkit::proves!("autonomy.bootstrap.absent_url_binds_no_redis_backend");

        let ladder = assemble_ladder::<Vec<u8>>(&empty()).expect("assemble");
        assert_eq!(ladder.tiers.len(), 6);
        assert!(ladder.hot_rungs.is_empty());
        assert!(ladder.rebinder.is_none());
        assert_eq!(ladder.report.l3, Some(L3Binding::NotConfigured));
        // The rung that exists is honest about what it is.
        assert_eq!(
            ladder.tiers[3].backend(),
            crate::tier::BackendKind::InMemoryFallback
        );
    }

    #[cfg(feature = "redis")]
    #[test]
    fn an_unreachable_server_degrades_loudly_and_rebindably() {
        testkit::proves!("autonomy.bootstrap.degraded_binding_is_reported_loudly");

        // Port 1 refuses immediately without a server anywhere nearby.
        let env = empty().with(ENV_REDIS_URL, "redis://127.0.0.1:1/");
        let ladder = assemble_ladder::<Vec<u8>>(&env).expect("assemble");
        // `expect` (allowed in this crate's tests) is the divergence of choice
        // here: a binding of the wrong shape fails the test loudly without a
        // `panic!` macro, which the production-code deny list refuses.
        let degraded = match ladder.report.l3 {
            Some(L3Binding::RedisDegradedToMemory {
                url,
                valkey,
                reason,
            }) => Some((url, valkey, reason)),
            _ => None,
        };
        let (url, valkey, reason) = degraded.expect("a loudly degraded binding");
        assert_eq!(url, "redis://127.0.0.1:1/");
        assert!(!valkey);
        assert!(
            !reason.contains("redis://"),
            "the reason must not echo the URL: credentials live there"
        );

        // Degraded-but-rebindable: the rung is a hot rung and a rebinder exists,
        // so when the server appears the governor puts it in.
        assert_eq!(ladder.hot_rungs.len(), 1);
        assert_eq!(ladder.hot_rungs[0].rung(), crate::tier::TierId::L3);
        assert!(ladder.rebinder.is_some());
        assert_eq!(
            ladder.tiers[3].backend(),
            crate::tier::BackendKind::InMemoryFallback
        );
    }

    #[cfg(feature = "redis")]
    #[tokio::test]
    async fn the_rebinder_refuses_every_rung_that_is_not_its_own() {
        let env = empty().with(ENV_REDIS_URL, "redis://127.0.0.1:1/");
        let ladder = assemble_ladder::<Vec<u8>>(&env).expect("assemble");
        let rebinder = ladder.rebinder.expect("degraded ladder carries a rebinder");
        assert!(
            rebinder(crate::tier::TierId::L4)
                .expect("declines cleanly")
                .is_none()
        );
        assert!(
            rebinder(crate::tier::TierId::L6)
                .expect("declines cleanly")
                .is_none()
        );
    }
}
