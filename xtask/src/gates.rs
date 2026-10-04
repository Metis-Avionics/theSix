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
        GateKind::Tracker => {
            // Not a subprocess. `--dry-run` still prints something meaningful so
            // the gate is reviewable in the matrix listing like every other one.
            cmd.push("xtask".to_string());
            cmd.push(format!("internal:{}", gate.name));
        }
    }
    cmd
}

/// How many findings the tracker is required to hold, per the contract.
#[derive(Debug, Clone, Copy)]
pub struct ExpectedFindings {
    pub blocking: Option<usize>,
    pub should_fix: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub root: PathBuf,
    /// Expected merge-readiness counts, read from the contract. `None` when the
    /// runner was invoked without one, in which case the gate reports
    /// `Unavailable` rather than trusting the tracker's self-declaration.
    pub expected: Option<ExpectedFindings>,
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

    if gate.kind == GateKind::Tracker {
        return merge_readiness(&gate.name, &opts.root, started, opts.expected.as_ref());
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

// ---------------------------------------------------------------------------
// Merge readiness
// ---------------------------------------------------------------------------

/// The parts of `bugs.toml` this gate reads. Everything else in the file is
/// documentation for humans and is deliberately not deserialised: a tracker
/// whose schema is enforced in full would fail on a new optional field, and a
/// tracker that fails to parse cannot report the bugs it exists to report.
#[derive(Debug, serde::Deserialize)]
struct BugFile {
    #[serde(default)]
    meta: BugMeta,
    #[serde(default)]
    bug: Vec<BugEntry>,
}

/// The expected finding counts, declared by the tracker itself.
///
/// This is what stops the gate being defused by deletion. A tracker that can be
/// emptied to make a readiness check pass is not a tracker, it is a switch — and
/// the first version of this gate had exactly that hole, found by
/// `empty_tracker_is_rejected`.
#[derive(Debug, Default, serde::Deserialize)]
struct BugMeta {
    #[serde(default)]
    findings_blocking: Option<usize>,
    #[serde(default)]
    findings_should_fix: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
struct BugEntry {
    id: String,
    title: String,
    status: String,
    #[serde(default)]
    blocks_merge: bool,
    /// Required for `status = "accepted-risk"`.
    #[serde(default)]
    accepted_by: Option<String>,
    /// Required for `status = "accepted-risk"` and for `status = "resolved"`.
    #[serde(default)]
    rationale: Option<String>,
}

/// Fail while any `blocks_merge` finding is still open.
///
/// The point of this gate is that it *can* fail. A readiness check that always
/// passes is the same defect this revision exists to remove, so there is
/// deliberately no "skip if inconvenient" path and no blanket allowlist.
///
/// A finding may leave `open` status only by being resolved, or by being
/// explicitly accepted as risk with a named owner and a written rationale —
/// never by editing the flag to `false`, because `tests/contract` requires every
/// finding to declare `blocks_merge` and records the count it expects.
///
/// A missing or unparseable `bugs.toml` is reported `Unavailable` (exit 3), not
/// `Passed`. An absent tracker is not evidence of readiness.
fn merge_readiness(
    name: &str,
    root: &Path,
    started: Instant,
    expected: Option<&ExpectedFindings>,
) -> GateResult {
    let done = |outcome: Outcome, detail: String| GateResult {
        name: name.to_string(),
        outcome,
        duration: started.elapsed(),
        detail: Some(detail),
    };

    let path = root.join("bugs.toml");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return done(
            Outcome::Unavailable,
            format!(
                "{} is absent; merge readiness cannot be verified",
                path.display()
            ),
        );
    };
    let file: BugFile = match toml::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            return done(
                Outcome::Unavailable,
                format!("{} does not parse: {e}", path.display()),
            );
        }
    };

    // Counts first: a tracker that has been emptied or trimmed must fail before
    // any finding is considered cleared.
    let actual_blocking = file.bug.iter().filter(|b| b.blocks_merge).count();
    let actual_should_fix = file.bug.iter().filter(|b| !b.blocks_merge).count();
    let mut count_problems = Vec::new();
    // The contract is the authority on how many findings should exist. The
    // tracker's own `[meta]` counts are informational and deliberately not
    // trusted: a file that declares its own expected contents can be edited to
    // agree with whatever it currently contains.
    let Some(expected) = expected else {
        return done(
            Outcome::Unavailable,
            "no merge-readiness expectations supplied by the contract".to_string(),
        );
    };
    if let Some(want) = expected.blocking {
        if want != actual_blocking {
            count_problems.push(format!(
                "theSix.toml expects {want} blocking finding(s), tracker holds {actual_blocking}"
            ));
        }
    }
    if let Some(want) = expected.should_fix {
        if want != actual_should_fix {
            count_problems.push(format!(
                "theSix.toml expects {want} should-fix finding(s), tracker holds {actual_should_fix}"
            ));
        }
    }
    if count_problems.is_empty() && file.bug.is_empty() {
        count_problems.push("tracker holds no findings at all".to_string());
    }
    if !count_problems.is_empty() {
        return done(
            Outcome::Failed,
            format!(
                "bugs.toml does not match its own declared counts:\n{}",
                count_problems
                    .iter()
                    .map(|p| format!("  {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        );
    }

    let mut blocking = Vec::new();
    for entry in &file.bug {
        if !entry.blocks_merge {
            continue;
        }
        let blank = |s: &Option<String>| s.as_ref().is_none_or(|v| v.trim().is_empty());
        let problem = match entry.status.as_str() {
            "open" => Some("still open".to_string()),
            "accepted-risk" => {
                if blank(&entry.accepted_by) || blank(&entry.rationale) {
                    Some("accepted-risk without both `accepted_by` and `rationale`".to_string())
                } else {
                    None
                }
            }
            "resolved" => {
                if blank(&entry.rationale) {
                    Some("resolved without a `rationale` recording how".to_string())
                } else {
                    None
                }
            }
            other => Some(format!(
                "unknown status {other:?}; expected open, resolved or accepted-risk"
            )),
        };
        if let Some(problem) = problem {
            blocking.push(format!("  {}  {problem}\n      {}", entry.id, entry.title));
        }
    }

    if blocking.is_empty() {
        let tracked = file.bug.iter().filter(|b| b.blocks_merge).count();
        return done(
            Outcome::Passed,
            format!("no blocking findings; {tracked} merge-blocking findings tracked, all cleared"),
        );
    }

    let count = blocking.len();
    done(
        Outcome::Failed,
        format!(
            "{} blocking finding(s) in bugs.toml:\n{}",
            count,
            blocking.join("\n")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::{ExpectedFindings, Outcome, merge_readiness};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    /// A fresh directory per call, so parallel tests cannot see each other's
    /// fixtures. `cargo nextest` runs each test in its own process, but the unit
    /// tests here may also run under plain `cargo test`.
    fn fixture(body: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "thesix-mr-{}-{}-{n}",
            std::process::id(),
            body.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(dir.join("bugs.toml"), body).expect("fixture write");
        dir
    }

    /// Expectations as the contract would supply them.
    fn expected(blocking: usize, should_fix: usize) -> ExpectedFindings {
        ExpectedFindings {
            blocking: Some(blocking),
            should_fix: Some(should_fix),
        }
    }

    fn check(body: &str) -> Outcome {
        // Expectations are derived from the fixture itself unless a test cares,
        // so a count assertion tests the count check rather than the fixture.
        let n_blocking = body.matches("blocks_merge = true").count();
        let n_soft = body.matches("blocks_merge = false").count();
        check_with(body, expected(n_blocking, n_soft))
    }

    fn check_with(body: &str, want: ExpectedFindings) -> Outcome {
        let dir = fixture(body);
        let r = merge_readiness("merge_readiness", &dir, Instant::now(), Some(&want));
        let _ = std::fs::remove_dir_all(&dir);
        r.outcome
    }

    fn resolved(id: &str) -> String {
        format!(
            "[[bug]]\nid = \"{id}\"\ntitle = \"t\"\nstatus = \"resolved\"\nblocks_merge = true\nrationale = \"how\"\n"
        )
    }

    fn blocking(id: &str, status: &str) -> String {
        format!(
            "[[bug]]\nid = \"{id}\"\ntitle = \"t\"\nstatus = \"{status}\"\nblocks_merge = true\n"
        )
    }

    /// The gate exists to fail. A test that only ever exercises the passing path
    /// would not notice if the check were removed entirely.
    #[test]
    fn open_blocking_finding_fails() {
        assert_eq!(check(&blocking("B1", "open")), Outcome::Failed);
    }

    #[test]
    fn several_open_findings_all_fail_and_are_named() {
        let r = merge_readiness(
            "merge_readiness",
            &fixture(&format!(
                "{}{}{}",
                blocking("B1", "open"),
                blocking("B2", "open"),
                blocking("B3", "open")
            )),
            Instant::now(),
            Some(&expected(3, 0)),
        );
        assert_eq!(r.outcome, Outcome::Failed);
        let detail = r.detail.expect("a failing gate must explain itself");
        for id in ["B1", "B2", "B3"] {
            assert!(detail.contains(id), "{id} missing from {detail}");
        }
    }

    #[test]
    fn resolved_with_rationale_passes() {
        assert_eq!(
            check(&format!(
                "[[bug]]\nid = \"B1\"\ntitle = \"t\"\nstatus = \"resolved\"\nblocks_merge = true\nrationale = \"guard added\"\n"
            )),
            Outcome::Passed
        );
    }

    /// A resolved finding with no record of how is a claim, not a resolution.
    #[test]
    fn resolved_without_rationale_fails() {
        assert_eq!(check(&blocking("B1", "resolved")), Outcome::Failed);
    }

    #[test]
    fn accepted_risk_with_owner_and_rationale_passes() {
        assert_eq!(
            check(
                "[[bug]]\nid = \"B1\"\ntitle = \"t\"\nstatus = \"accepted-risk\"\nblocks_merge = true\naccepted_by = \"leo\"\nrationale = \"risk accepted for 2.0\"\n"
            ),
            Outcome::Passed
        );
    }

    #[test]
    fn accepted_risk_without_rationale_fails() {
        assert_eq!(
            check(
                "[[bug]]\nid = \"B1\"\ntitle = \"t\"\nstatus = \"accepted-risk\"\nblocks_merge = true\naccepted_by = \"leo\"\n"
            ),
            Outcome::Failed
        );
    }

    #[test]
    fn accepted_risk_without_owner_fails() {
        assert_eq!(
            check(
                "[[bug]]\nid = \"B1\"\ntitle = \"t\"\nstatus = \"accepted-risk\"\nblocks_merge = true\nrationale = \"why\"\n"
            ),
            Outcome::Failed
        );
    }

    /// Clearing the flag is not a path out: `no_cross_key_corruption`-style
    /// downgrades have to be visible, and a non-blocking finding still passes so
    /// the B4/B5 class of fix cannot be used to silence this gate wholesale.
    #[test]
    fn non_blocking_finding_passes_regardless_of_status() {
        assert_eq!(
            check("[[bug]]\nid = \"B4\"\ntitle = \"t\"\nstatus = \"open\"\nblocks_merge = false\n"),
            Outcome::Passed
        );
    }

    /// A typo must not read as "cleared".
    #[test]
    fn unknown_status_fails() {
        assert_eq!(check(&blocking("B1", "fixed")), Outcome::Failed);
        assert_eq!(check(&blocking("B1", "OPEN")), Outcome::Failed);
    }

    /// An absent tracker is not evidence of readiness. Reporting `Unavailable`
    /// is what makes `xtask` exit 3 rather than 0.
    #[test]
    fn absent_tracker_is_unavailable_not_passed() {
        let dir = std::env::temp_dir().join(format!("thesix-mr-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let r = merge_readiness(
            "merge_readiness",
            &dir,
            Instant::now(),
            Some(&expected(0, 0)),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.outcome, Outcome::Unavailable);
    }

    #[test]
    fn malformed_tracker_is_unavailable_not_passed() {
        assert_eq!(check("this is not = = toml"), Outcome::Unavailable);
    }

    /// An empty tracker is a vacuous pass, so it is rejected rather than
    /// reported ready. A tracker that has lost its findings must fail loudly.
    #[test]
    fn empty_tracker_is_rejected() {
        assert_eq!(check_with("[meta]\n", expected(3, 2)), Outcome::Failed);
    }

    /// Deleting a finding must not defuse the gate. The tracker declares its own
    /// expected counts, so a removal shows up as a mismatch rather than as
    /// progress.
    #[test]
    fn deleting_a_finding_fails_the_count_check() {
        let body = format!(
            "[meta]\nfindings_blocking = 3\nfindings_should_fix = 2\n{}{}",
            resolved("B1"),
            resolved("B2")
        );
        // B3 has been quietly removed; the contract still expects three.
        assert_eq!(check_with(&body, expected(3, 2)), Outcome::Failed);
    }

    #[test]
    fn downgrading_severity_to_dodge_the_gate_fails() {
        // B1 flipped to non-blocking to escape the check.
        let body = format!(
            "[meta]\nfindings_blocking = 0\nfindings_should_fix = 1\n[[bug]]\nid = \"B1\"\ntitle = \"t\"\nstatus = \"open\"\nblocks_merge = false\n"
        );
        assert_eq!(check_with(&body, expected(3, 2)), Outcome::Failed);
    }

    #[test]
    fn counts_matching_with_cleared_findings_passes() {
        let body = format!(
            "[meta]\nfindings_blocking = 2\nfindings_should_fix = 1\n{}{}{}",
            resolved("B1"),
            "[[bug]]\nid = \"B1b\"\ntitle = \"t\"\nstatus = \"resolved\"\nblocks_merge = true\nrationale = \"how\"\n",
            "[[bug]]\nid = \"B4\"\ntitle = \"t\"\nstatus = \"open\"\nblocks_merge = false\n"
        );
        assert_eq!(check(&body), Outcome::Passed);
    }
}
