//! Gate execution.
//!
//! A gate is a named, contract-declared verification step. Execution is
//! deliberately boring: build a `std::process::Command`, stream its output,
//! record how long it took and whether it passed. The interesting decisions are
//! all in the environment setup, because that is what makes an eight-core
//! machine finish the matrix in minutes instead of an hour.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::contract::{Contract, Gate, GateKind};

/// The crate under verification.
pub const CRATE: &str = "thesix";

/// The fault taxonomy, read from the crate rather than restated here.
///
/// Duplicating this list would defeat the purpose of the cross-check: a copy in
/// the runner and a copy in the contract can disagree with the implementation
/// and still validate. Deriving it from `FaultClass::ALL` means adding a fault
/// to the implementation without updating the contract fails the gate.
#[must_use]
pub fn fault_classes() -> Vec<&'static str> {
    thesix::FaultClass::ALL.iter().map(|c| c.as_str()).collect()
}

/// Toolchain accelerations, probed once and reported so a surprising slow run
/// has an explanation.
#[derive(Debug, Default, Clone)]
pub struct Toolchain {
    pub sccache: Option<PathBuf>,
    pub linker: Option<PathBuf>,
    pub mold: Option<PathBuf>,
    pub nextest: bool,
}

impl Toolchain {
    pub fn probe() -> Self {
        let which = |name: &str| which_bin(name);
        // A linker is only worth selecting if the matching linker driver is
        // actually present; setting `linker = clang` without clang installed
        // turns every link into a confusing exec failure.
        let mold = which("mold");
        let linker = if mold.is_some() { which("clang") } else { None };
        Self {
            sccache: which("sccache"),
            linker,
            mold,
            nextest: which("cargo-nextest").is_some() || which("cargo").is_some(),
        }
    }

    /// One-line human summary for the run header.
    #[must_use]
    pub fn summary(&self) -> String {
        let link = match (&self.linker, &self.mold) {
            (Some(c), Some(m)) => format!(
                "{} + {}",
                c.file_name().unwrap_or_default().to_string_lossy(),
                m.file_name().unwrap_or_default().to_string_lossy()
            ),
            (Some(c), None) => c
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            _ => "system default".to_string(),
        };
        format!(
            "crate: {CRATE} | runner: {} | sccache: {} | linker: {link}",
            if self.nextest {
                "nextest"
            } else {
                "cargo test"
            },
            if self.sccache.is_some() { "on" } else { "off" }
        )
    }

    /// Environment applied to every cargo invocation.
    ///
    /// The mold flags go in `CARGO_TARGET_*_RUSTFLAGS` rather than
    /// `.cargo/config.toml` so the speedup follows the runner instead of the
    /// checkout: a contributor without mold installed still gets a working
    /// build rather than a link error.
    #[must_use]
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(s) = &self.sccache {
            env.push(("RUSTC_WRAPPER".to_string(), s.display().to_string()));
            // Keep the cache bounded; the matrix builds the same units under
            // several feature combinations.
            env.push(("SCCACHE_CACHE_SIZE".to_string(), "20G".to_string()));
        }
        if let Some(linker) = &self.linker {
            env.push((
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER".to_string(),
                linker.display().to_string(),
            ));
        }
        if self.mold.is_some() {
            let existing = std::env::var("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS")
                .unwrap_or_default();
            let flag = "-Clink-arg=-fuse-ld=mold";
            let combined = if existing.is_empty() {
                flag.to_string()
            } else {
                format!("{existing} {flag}")
            };
            env.push((
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_string(),
                combined,
            ));
        }
        env
    }
}

fn which_bin(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    /// A required tool is absent, so the gate could not honestly run. Reported
    /// separately from Failed because silently skipping a gate is exactly the
    /// vacuous-pass failure mode the contract forbids.
    Unavailable,
}

#[derive(Debug)]
pub struct GateResult {
    pub name: String,
    pub outcome: Outcome,
    pub duration: Duration,
    pub detail: Option<String>,
}

/// Build the argv for a gate without running it. Exposed so `--dry-run` can
/// print exactly what would be executed, which is the only way to review a
/// 23-gate matrix without running it.
#[must_use]
pub fn argv_for(gate: &Gate, _toolchain: &Toolchain) -> Vec<String> {
    let mut cmd: Vec<String> = Vec::new();
    match gate.kind {
        GateKind::Tool | GateKind::Nightly => {
            cmd.extend(gate.command.iter().cloned());
        }
        GateKind::Cargo => {
            cmd.push("cargo".to_string());
            cmd.extend(gate.args.iter().cloned());
        }
        GateKind::Doctest => {
            cmd.push("cargo".to_string());
            cmd.push("test".to_string());
            cmd.push("--doc".to_string());
            cmd.extend(gate.features.iter().cloned());
        }
        GateKind::Test | GateKind::Slow => {
            let mut inner = vec![
                "cargo".to_string(),
                "nextest".to_string(),
                "run".to_string(),
                // Keep going after a failing target so one run reports every
                // broken layer rather than only the first.
                "--no-fail-fast".to_string(),
                "--hide-progress-bar".to_string(),
            ];
            inner.extend(gate.features.iter().cloned());
            for t in &gate.targets {
                inner.push("--test".to_string());
                inner.push(t.clone());
            }
            if gate.kind == GateKind::Slow {
                // `#[ignore]`d gates: opt in explicitly, and only these.
                inner.push("--run-ignored".to_string());
                inner.push("only".to_string());
            }
            cmd = inner;
        }
    }
    cmd
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub root: PathBuf,
    pub toolchain: Toolchain,
    pub dry_run: bool,
    pub verbose: bool,
}

/// Execute one gate.
pub fn run(gate: &Gate, opts: &RunOptions) -> GateResult {
    let argv = argv_for(gate, &opts.toolchain);
    debug_assert!(!argv.is_empty(), "every gate kind produces an argv");
    let started = Instant::now();

    if opts.dry_run {
        return GateResult {
            name: gate.name.clone(),
            outcome: Outcome::Passed,
            duration: Duration::ZERO,
            detail: Some(format!("(dry run) {}", argv.join(" "))),
        };
    }

    let (program, args) = argv.split_first().expect("argv_for never returns empty");
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&opts.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in opts.toolchain.env() {
        cmd.env(k, v);
    }
    for (k, v) in &gate.env {
        cmd.env(k, v);
    }
    // Colour only when a human is watching.
    cmd.env(
        "CARGO_TERM_COLOR",
        if opts.verbose { "always" } else { "never" },
    );

    let unavailable = missing_tool(program, &argv);
    if let Some(missing) = unavailable {
        return GateResult {
            name: gate.name.clone(),
            outcome: Outcome::Unavailable,
            duration: started.elapsed(),
            detail: Some(format!("`{program}` is not installed ({missing})")),
        };
    }

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            return GateResult {
                name: gate.name.clone(),
                outcome: Outcome::Failed,
                duration: started.elapsed(),
                detail: Some(format!("failed to spawn `{program}`: {e}")),
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    if !opts.verbose || !output.status.success() {
        // Surface compiler and test diagnostics; silence is worse than noise
        // when something has just failed.
        let mut w = std::io::stderr().lock();
        let _ = w.write_all(combined.as_bytes());
        let _ = w.flush();
    }

    GateResult {
        name: gate.name.clone(),
        outcome: if output.status.success() {
            Outcome::Passed
        } else {
            Outcome::Failed
        },
        duration: started.elapsed(),
        detail: summarize(&combined, output.status.code()),
    }
}

/// Whether a gate's tooling is present.
///
/// Only the *head* of the command is checked. A `cargo <sub>` form is not probed
/// for a `cargo-<sub>` binary, because most of cargo's subcommands are built in:
/// the first version of this looked for `cargo-check` and `cargo-package`, found
/// neither, and reported fifteen healthy gates as "tool not installed".
///
/// Third-party cargo subcommands *are* checked by name, since those genuinely
/// need a separate binary and their absence is the difference between "verified"
/// and "silently skipped".
fn missing_tool(program: &str, argv: &[String]) -> Option<String> {
    if program == "cargo" {
        // Skip a leading `+toolchain` selector, so `cargo +nightly fuzz` is probed
        // as `fuzz` rather than as an unknown subcommand. Without this the nightly
        // gate reported "failed" when the honest answer was "cargo-fuzz is not
        // installed" — which is a different thing, and the contract cares about the
        // difference.
        let sub = argv
            .iter()
            .skip(1)
            .find(|a| !a.starts_with('+'))
            .map(String::as_str);
        let Some(sub) = sub else {
            return which_bin("cargo")
                .is_none()
                .then(|| "`cargo` is not installed".to_string());
        };
        // Subcommands that are separate binaries rather than cargo built-ins.
        const EXTERNAL: &[&str] = &[
            "nextest",
            "deny",
            "machete",
            "fuzz",
            "audit",
            "miri",
            "llvm-cov",
            "tarpaulin",
        ];
        if EXTERNAL.contains(&sub) {
            let binary = format!("cargo-{sub}");
            return which_bin(&binary)
                .is_none()
                .then(|| format!("`{binary}` is not installed"));
        }
        return None;
    }

    if program.starts_with("cargo-") {
        return which_bin(program)
            .is_none()
            .then(|| format!("`{program}` is not installed"));
    }

    if program.contains('/') || program.ends_with("cargo") {
        return None;
    }
    which_bin(program)
        .is_none()
        .then(|| format!("`{program}` is not installed"))
}

/// Pull the most useful line out of a successful run.
fn summarize(output: &str, code: Option<i32>) -> Option<String> {
    let interesting = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| {
            !l.is_empty()
                && (l.contains("Summary")
                    || l.contains("test result")
                    || l.contains("passed")
                    || l.contains("Checking")
                    || l.contains("Finished")
                    || l.contains("error")
                    || l.contains("warning"))
        })
        .map(|l| {
            let l = l.trim_start_matches("test result: ");
            if l.chars().count() > 110 {
                let mut s: String = l.chars().take(107).collect();
                s.push_str("...");
                s
            } else {
                l.to_string()
            }
        });
    interesting.or_else(|| code.map(|c| format!("exit code {c}")))
}

/// Every gate, resolved against the contract.
pub fn resolve<'a>(
    contract: &'a Contract,
    selector: Option<&str>,
) -> Result<Vec<&'a Gate>, String> {
    match selector {
        None => Ok(contract.default_pass()),
        Some("all") => Ok(contract.gates.iter().collect()),
        Some(name) => contract.gate(name).map(|g| vec![g]).ok_or_else(|| {
            let names: Vec<&str> = contract.gates.iter().map(|g| g.name.as_str()).collect();
            format!("no gate named {name:?}. known gates: {}", names.join(", "))
        }),
    }
}

/// True when `path` looks like a test target directory we can look for.
#[must_use]
pub fn test_target_exists(root: &Path, target: &str) -> bool {
    let dir = root.join("tests").join(target);
    dir.join("main.rs").is_file() || root.join("tests").join(format!("{target}.rs")).is_file()
}
