//! `theSix.toml` loader and structural validator.
//!
//! The contract is the source of truth. The runner refuses to execute a gate it
//! cannot find in the contract, and refuses to start if the contract names a
//! gate the runner has no strategy for. That two-way check is the point: it is
//! what stops the contract and the runner from drifting apart the way the six
//! previous `specs/*.toml` files did.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// How a gate is executed. Must match the `kind` spelling in the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    /// An external command, run verbatim.
    Tool,
    /// A `cargo` invocation with the given args.
    Cargo,
    /// A nextest run over the listed test targets.
    Test,
    /// `cargo test --doc`.
    Doctest,
    /// `#[ignore]`d tests, excluded from the default pass.
    Slow,
    /// Requires a nightly toolchain, excluded from the default pass.
    Nightly,
    /// Executed by the runner itself rather than a subprocess. Reads a repository
    /// artefact and decides pass/fail from its contents.
    Tracker,
    /// Executed by the runner itself rather than a subprocess. Reads commit
    /// metadata and decides pass/fail from it.
    Authorship,
    /// Executed by the runner itself rather than a subprocess. Reads a
    /// repository artefact, parses the source tree, and decides pass/fail from
    /// whether what it found still matches what it was told to expect.
    Analysis,
}

impl GateKind {
    /// Whether this gate participates in the default `xtask gates` pass.
    #[must_use]
    pub fn in_default_pass(self) -> bool {
        !matches!(self, Self::Slow | Self::Nightly)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Cargo => "cargo",
            Self::Test => "test",
            Self::Doctest => "doctest",
            Self::Slow => "slow",
            Self::Nightly => "nightly",
            Self::Tracker => "tracker",
            Self::Authorship => "authorship",
            Self::Analysis => "analysis",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Gate {
    pub name: String,
    pub kind: GateKind,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub targets: Vec<String>,
    /// Feature flags forwarded to cargo/nextest, e.g. `["--all-features"]`.
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub description: String,
}

/// The merge-readiness contract: how many findings `bugs.toml` is expected to
/// hold. Kept in the contract rather than in the tracker, because a tracker that
/// declares its own expected contents can be defused by editing itself — the
/// counts have to live somewhere the tracker does not control.
#[derive(Debug, Default, Deserialize)]
struct MergeReadiness {
    /// Path to the findings tracker, relative to the repository root. Read by the
    /// `merge_readiness` gate rather than hardcoded, so moving the tracker is a
    /// contract edit that the gate follows instead of silently ignoring.
    #[serde(default = "default_tracker")]
    tracker: String,
    #[serde(default)]
    expected_blocking: Option<usize>,
    #[serde(default)]
    expected_should_fix: Option<usize>,
}

/// The tracker path assumed when the contract does not declare one. Matches what
/// `theSix.toml` ships so an omitted key behaves like the declared one rather than
/// failing late.
fn default_tracker() -> String {
    "bugs.toml".to_string()
}

/// The trailer keys the `authorship` gate rejects.
///
/// Declared as *keys*, never as attributions. A gate that named the specific
/// string it forbids would put that string in its own source, so the rule has to
/// be expressible without it — and a key is enough, because the thing worth
/// banning is the act of adding a co-author trailer, not one particular value.
#[derive(Debug, Default, Deserialize)]
struct Attribution {
    #[serde(default)]
    forbidden_trailers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GateOrder {
    order: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Verification {
    #[serde(rename = "gate")]
    gates: Vec<Gate>,
    #[serde(default)]
    merge_readiness: MergeReadiness,
    #[serde(default)]
    attribution: Attribution,
    gate_order: GateOrder,
    verification_required: bool,
    anti_vacuity_required: bool,
    required: VerificationRequired,
}

#[derive(Debug, Deserialize)]
struct VerificationRequired {
    fmt: bool,
    check_all_targets: bool,
    check_all_features: bool,
    tests_all_targets: bool,
    tests_all_features: bool,
    doctests: bool,
    clippy_warnings_as_errors: bool,
    documentation: bool,
    dependency_audit: bool,
    dependency_hygiene: bool,
    package_validation: bool,
}

#[derive(Debug, Deserialize)]
struct Spec {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct Layers {
    #[serde(flatten)]
    map: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Negative {
    cases: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct FaultInjection {
    faults: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PropertyTesting {
    #[serde(default)]
    invariants: BTreeMap<String, bool>,
}

#[derive(Debug, Deserialize)]
struct Fuzz {
    #[serde(default)]
    targets: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RecoveryTesting {
    #[serde(default)]
    requirements: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DurabilityTesting {
    #[serde(default)]
    events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SecurityTesting {
    #[serde(default)]
    requirements: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ObservabilityOperation {
    #[serde(default)]
    fields: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Observability {
    operation: ObservabilityOperation,
}

#[derive(Debug, Deserialize)]
struct Testing {
    layers: Layers,
    negative: Negative,
    fault_injection: FaultInjection,
    property: PropertyTesting,
    fuzz: Fuzz,
    recovery: RecoveryTesting,
    durability: DurabilityTesting,
    security: SecurityTesting,
}

#[derive(Debug, Deserialize)]
struct ContractFile {
    spec: Spec,
    verification: Verification,
    testing: Testing,
    observability: Observability,
}

/// A validated contract.
#[derive(Debug)]
pub struct Contract {
    pub path: PathBuf,
    pub version: String,
    pub gates: Vec<Gate>,
    pub layers: BTreeMap<String, Vec<String>>,
    pub negative_cases: Vec<String>,
    pub faults: Vec<String>,
    pub property_invariants: BTreeMap<String, bool>,
    pub fuzz_targets: Vec<String>,
    pub recovery_requirements: Vec<String>,
    pub durability_events: Vec<String>,
    pub security_requirements: Vec<String>,
    pub telemetry_fields: Vec<String>,
    /// Every feature flag the runner will pass, unioned across gates.
    pub required_booleans: Vec<(String, bool)>,
    /// Expected merge-readiness finding counts, declared by the contract so the
    /// tracker cannot be defused by editing itself.
    pub expected_findings: crate::gates::ExpectedFindings,
    /// Tracker path as declared by `[verification.merge_readiness].tracker`.
    pub tracker: String,
    /// Trailer keys the `authorship` gate rejects, from
    /// `[verification.attribution].forbidden_trailers`.
    pub forbidden_trailers: Vec<String>,
}

impl Contract {
    /// Load and validate. Every failure here is fatal: running gates against a
    /// contract that does not parse would mean verifying nothing in particular.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let parsed: ContractFile =
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;

        let mut problems = Vec::new();

        if parsed.spec.name != "theSix" {
            problems.push(format!(
                "[spec].name is {:?}, expected \"theSix\"",
                parsed.spec.name
            ));
        }

        // Gate order must agree with declaration order. A mismatch means the
        // file was hand-edited and one of the two is lying about sequence.
        let declared: Vec<&str> = parsed
            .verification
            .gates
            .iter()
            .map(|g| g.name.as_str())
            .collect();
        let ordered = parsed.verification.gate_order.order.clone();
        if declared.len() != ordered.len() {
            problems.push(format!(
                "[verification.gate_order] lists {} gates but {} are declared",
                ordered.len(),
                declared.len()
            ));
        }
        for (i, name) in ordered.iter().enumerate() {
            if declared.get(i) != Some(&name.as_str()) {
                problems.push(format!(
                    "gate order position {i} is {name:?} but the {i}th declared gate is {:?}",
                    declared.get(i).unwrap_or(&"<none>")
                ));
            }
        }

        // Duplicate gate names would silently shadow one another.
        let mut seen = BTreeMap::new();
        for g in &parsed.verification.gates {
            if seen.insert(g.name.clone(), ()).is_some() {
                problems.push(format!("duplicate gate name {:?}", g.name));
            }
            // A gate must carry the detail its kind needs, or the runner would
            // have to guess at execution time.
            match g.kind {
                GateKind::Tool | GateKind::Nightly if g.command.is_empty() => {
                    problems.push(format!(
                        "gate {:?} is kind=tool/nightly with no command",
                        g.name
                    ));
                }
                GateKind::Cargo if g.args.is_empty() => {
                    problems.push(format!("gate {:?} is kind=cargo with no args", g.name));
                }
                GateKind::Test | GateKind::Slow if g.targets.is_empty() => {
                    problems.push(format!(
                        "gate {:?} is kind={} with no targets",
                        g.name,
                        g.kind.as_str()
                    ));
                }
                _ => {}
            }
        }

        // The authorship gate and its policy are only meaningful together, and
        // both directions of the mismatch are defects.
        let declares_gate = parsed
            .verification
            .gates
            .iter()
            .any(|g| g.kind == GateKind::Authorship);
        let trailers = &parsed.verification.attribution.forbidden_trailers;
        if declares_gate && trailers.is_empty() {
            problems.push(
                "an `authorship` gate is declared but \
                 [verification.attribution].forbidden_trailers is empty. A gate with \
                 nothing to forbid cannot fail, so it reports success for a policy \
                 it is not enforcing."
                    .to_string(),
            );
        }
        if !declares_gate && !trailers.is_empty() {
            problems.push(
                "[verification.attribution] declares trailers to forbid but no \
                 `authorship` gate exists to enforce them. A policy nothing checks is \
                 a promise the runtime does not have to keep."
                    .to_string(),
            );
        }
        // Each key is compared against the text before a message line's first
        // colon, so a key containing whitespace or a colon could never match.
        // That is dead configuration that reads as coverage.
        for key in trailers {
            if key.is_empty() || key.contains(':') || key.chars().any(char::is_whitespace) {
                problems.push(format!(
                    "[verification.attribution].forbidden_trailers entry {key:?} is not a \
                     usable git trailer key. Use the bare key as git writes it — no colon, \
                     no surrounding whitespace."
                ));
            }
        }

        // Every test target named by a layer must be claimed by some gate, or the
        // layer is decorative.
        let claimed: std::collections::BTreeSet<&str> = parsed
            .verification
            .gates
            .iter()
            .flat_map(|g| g.targets.iter().map(String::as_str))
            .collect();
        for (layer, targets) in &parsed.testing.layers.map {
            for t in targets {
                if !claimed.contains(t.as_str()) {
                    problems.push(format!(
                        "layer {layer:?} names test target {t:?}, which no gate runs"
                    ));
                }
            }
        }

        // `[verification.required]` must all be true. A false there would let
        // the runner declare success while skipping something the contract says
        // is mandatory.
        let req = &parsed.verification.required;
        let required_booleans = vec![
            ("fmt".to_string(), req.fmt),
            ("check_all_targets".to_string(), req.check_all_targets),
            ("check_all_features".to_string(), req.check_all_features),
            ("tests_all_targets".to_string(), req.tests_all_targets),
            ("tests_all_features".to_string(), req.tests_all_features),
            ("doctests".to_string(), req.doctests),
            (
                "clippy_warnings_as_errors".to_string(),
                req.clippy_warnings_as_errors,
            ),
            ("documentation".to_string(), req.documentation),
            ("dependency_audit".to_string(), req.dependency_audit),
            ("dependency_hygiene".to_string(), req.dependency_hygiene),
            ("package_validation".to_string(), req.package_validation),
        ];
        for (flag, value) in &required_booleans {
            if !*value {
                problems.push(format!(
                    "[verification.required].{flag} is false, but the contract declares it mandatory"
                ));
            }
        }
        if !parsed.verification.verification_required {
            problems.push("[verification].verification_required is false".to_string());
        }
        if !parsed.verification.anti_vacuity_required {
            problems.push("[verification].anti_vacuity_required is false".to_string());
        }

        // The fault taxonomy in the lib and the contract must be the same list,
        // in the same order. A fault added to one and not the other is a gate
        // that either cannot run or runs untested.
        let lib_faults: Vec<&str> = crate::gates::fault_classes();
        if lib_faults != parsed.testing.fault_injection.faults {
            problems.push(format!(
                "[testing.fault_injection].faults does not match the crate's fault taxonomy.\n  contract: {:?}\n  crate:    {:?}",
                parsed.testing.fault_injection.faults, lib_faults
            ));
        }

        // Every `[testing.property.invariants]` entry must be enabled. A false
        // there would mean a named invariant is declared and not held.
        for (name, value) in &parsed.testing.property.invariants {
            if !*value {
                problems.push(format!(
                    "[testing.property.invariants].{name} is false, but the contract lists it \
                     as a required invariant"
                ));
            }
        }

        // The listed case/enum sets must not be empty: an empty list would let
        // every `required = true` section pass with zero coverage.
        for (label, len) in [
            (
                "[testing.negative].cases",
                parsed.testing.negative.cases.len(),
            ),
            (
                "[testing.fault_injection].faults",
                parsed.testing.fault_injection.faults.len(),
            ),
            ("[testing.fuzz].targets", parsed.testing.fuzz.targets.len()),
            (
                "[testing.property.invariants]",
                parsed.testing.property.invariants.len(),
            ),
            (
                "[testing.recovery].requirements",
                parsed.testing.recovery.requirements.len(),
            ),
            (
                "[testing.durability].events",
                parsed.testing.durability.events.len(),
            ),
            (
                "[testing.security].requirements",
                parsed.testing.security.requirements.len(),
            ),
            (
                "[observability.operation].fields",
                parsed.observability.operation.fields.len(),
            ),
        ] {
            if len == 0 {
                problems.push(format!("{label} is empty but its section is required"));
            }
        }

        if !problems.is_empty() {
            return Err(format!(
                "contract validation failed for {}:\n  - {}",
                path.display(),
                problems.join("\n  - ")
            ));
        }

        Ok(Self {
            path: path.to_path_buf(),
            version: parsed.spec.version,
            gates: parsed.verification.gates,
            layers: parsed.testing.layers.map,
            negative_cases: parsed.testing.negative.cases,
            faults: parsed.testing.fault_injection.faults,
            property_invariants: parsed.testing.property.invariants,
            fuzz_targets: parsed.testing.fuzz.targets,
            recovery_requirements: parsed.testing.recovery.requirements,
            durability_events: parsed.testing.durability.events,
            security_requirements: parsed.testing.security.requirements,
            telemetry_fields: parsed.observability.operation.fields,
            required_booleans,
            expected_findings: crate::gates::ExpectedFindings {
                blocking: parsed.verification.merge_readiness.expected_blocking,
                should_fix: parsed.verification.merge_readiness.expected_should_fix,
            },
            tracker: parsed.verification.merge_readiness.tracker.clone(),
            forbidden_trailers: parsed.verification.attribution.forbidden_trailers.clone(),
        })
    }

    #[must_use]
    pub fn gate(&self, name: &str) -> Option<&Gate> {
        self.gates.iter().find(|g| g.name == name)
    }

    /// Cross-check the contract against the checkout.
    ///
    /// A gate that names a test target which does not exist is the most
    /// dangerous kind of rot: it looks like coverage in review, and CI reports
    /// the gate as having run. `cargo nextest --test missing` fails loudly, but
    /// only once someone runs the gate, so this catches it at validation time
    /// instead.
    pub fn validate_targets(&self, root: &std::path::Path) -> Result<(), String> {
        let mut problems = Vec::new();
        for gate in &self.gates {
            for target in &gate.targets {
                if !crate::gates::test_target_exists(root, target) {
                    problems.push(format!(
                        "gate {:?} targets tests/{target}.rs or tests/{target}/main.rs, \
                         neither of which exists",
                        gate.name
                    ));
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "contract/repo mismatch:\n  - {}",
                problems.join("\n  - ")
            ))
        }
    }

    /// Counts declared by the layer sections, for the contract report.
    #[must_use]
    pub fn declared_case_counts(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("negative cases", self.negative_cases.len()),
            ("faults", self.faults.len()),
            ("property invariants", self.property_invariants.len()),
            ("fuzz targets", self.fuzz_targets.len()),
            ("recovery requirements", self.recovery_requirements.len()),
            ("durability events", self.durability_events.len()),
            ("security requirements", self.security_requirements.len()),
            ("telemetry fields", self.telemetry_fields.len()),
        ]
    }

    /// Gates in contract order.
    #[must_use]
    pub fn default_pass(&self) -> Vec<&Gate> {
        self.gates
            .iter()
            .filter(|g| g.kind.in_default_pass())
            .collect()
    }

    #[must_use]
    pub fn deferred(&self) -> Vec<&Gate> {
        self.gates
            .iter()
            .filter(|g| !g.kind.in_default_pass())
            .collect()
    }
}
