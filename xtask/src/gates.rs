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

/// Strip the environment `cargo run` injects into xtask's own process.
///
/// `cargo xtask run <gate>` is `cargo run --package xtask --`, and cargo exports a
/// pile of `CARGO_*` variables describing *xtask's own package* to the process it
/// runs. Anything xtask then spawns inherits them, so a gate that shells out to
/// another cargo tool is told the manifest directory is `xtask/` rather than the
/// repository root.
///
/// This is not hypothetical. `cargo machete` reads `CARGO_MANIFEST_DIR`, decided it
/// had been pointed at `xtask/`, and walked the wrong tree:
///
///     Analyzing dependencies of crates in machete...
///     Error: Errors when walking over directories:
///     machete: IO error for operation on machete: No such file or directory
///
/// The gate passed when xtask was invoked as `./target/debug/xtask`, which has none
/// of these variables, and failed under `cargo xtask` — which is the invocation the
/// README, `AGENTS.md` and the CI workflow all use. So the documented way to run a
/// gate was the broken one, and the way that happened to work was the undocumented
/// one. xtask is a gate runner, not a cargo subcommand: its children should see the
/// caller's environment.
///
/// `LD_LIBRARY_PATH` is left alone. It is cargo's mechanism for finding the freshly
/// built binary, and on the toolchain this repository targets it has caused no
/// observed misbehaviour; removing it would be a larger change than the evidence
/// supports.
fn scrub_cargo_env(cmd: &mut Command) {
    const FIXED: &[&str] = &[
        "CARGO",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_CRATE_NAME",
        "CARGO_BIN_NAME",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_TARGET_TMPDIR",
    ];
    // CARGO_PKG_* is an open set that grows with manifest fields, so enumerate it
    // rather than hard-coding a list that would rot.
    let pkg_keys: Vec<String> = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("CARGO_PKG_"))
        .collect();
    for key in FIXED {
        cmd.env_remove(key);
    }
    for key in &pkg_keys {
        cmd.env_remove(key);
    }
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
        GateKind::Tracker | GateKind::Authorship => {
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
    /// Tracker path as declared by `[verification.merge_readiness].tracker`, so
    /// the gate follows the contract instead of a hardcoded filename.
    pub tracker: String,
    /// Trailer keys the `authorship` gate rejects, as declared by
    /// `[verification.attribution].forbidden_trailers`.
    pub forbidden_trailers: Vec<String>,
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
        return merge_readiness(
            &gate.name,
            &opts.root,
            started,
            opts.expected.as_ref(),
            &opts.tracker,
        );
    }

    if gate.kind == GateKind::Authorship {
        return authorship(&gate.name, &opts.root, started, &opts.forbidden_trailers);
    }

    let (program, args) = argv.split_first().expect("argv_for never returns empty");
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&opts.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_cargo_env(&mut cmd);
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
// Commit authorship
// ---------------------------------------------------------------------------

/// Separators for parsing `git log` output: `%x1f` unit, `%x1e` record. These are
/// the two git itself uses for `--format`, and they are what `git
/// interpret-trailers` round-trips, so they are the least surprising choice
/// available.
///
/// A commit message is free text, so any delimiter is theoretically forgeable.
/// `splitn(3, ..)` is what makes that harmless: a message containing a separator
/// keeps it in the message field instead of shifting the commit boundary and
/// hiding the very line being scanned.
const FIELD_SEP: char = '\u{1f}';
const RECORD_SEP: char = '\u{1e}';

/// Reduce a trailer key to a canonical form: lowercase, with `-`, `_` and
/// whitespace removed.
///
/// `Co-Authored-By`, `co-authored-by`, `CoAuthoredBy` and `Co_Authored_By` all
/// collapse to the same key. Without this the contract would have to list every
/// spelling, and a list of spellings is only as good as the author's imagination
/// — a ban that misses the variant someone actually typed is not a ban.
fn normalise_key(key: &str) -> String {
    key.chars()
        .filter(|c| *c != '-' && *c != '_' && !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Replace every banned-trailer line in `text` with a placeholder.
///
/// The offending commit is reported by id *and* subject, because naming the
/// subject is what makes a 50-commit history cheap to clean. But a commit whose
/// whole message is the banned line — which is what a tool that appends the
/// trailer as its own message produces — would otherwise have the gate reprint the
/// attribution in its own output, undoing the thing it exists to prevent. So the
/// subject is redacted whenever it is itself the violation.
///
/// Redacting is not a loss of information: `git log -1 <id>` shows the message in
/// full to whoever has the repository, and the gate's job is to fail and point at
/// the commit, not to become a second copy of the history.
fn redact_banned(text: &str, wanted: &[(String, &str)]) -> String {
    text.lines()
        .map(|line| match line.split_once(':') {
            Some((key, _)) if wanted.iter().any(|(w, _)| *w == normalise_key(key)) => {
                "<withheld: banned trailer>"
            }
            _ => line,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fail while any commit in this repository's history carries a banned trailer.
///
/// The rule is on the trailer *key*, and the banned keys live in the contract, so
/// no attribution appears in this file in order to check for one. The offending
/// value is deliberately not echoed: the commit id is reported instead, which is
/// all that is needed to fix it (`git log -1 <id>`) and keeps the gate's output
/// free of the string it exists to keep out of the history.
///
/// Every way of *not* being able to check is reported `Unavailable` rather than
/// `Passed`, which is what makes `xtask` exit 3:
///
/// * no `git` on PATH;
/// * `root` is not inside a work tree — a published crate's `.crate` tarball, or
///   a source export, has no history to scan;
/// * a shallow clone, where the visible history is a truncation of the real one
///   and a scan would report a pass over one commit;
/// * the contract declares no forbidden trailers, so there is nothing to enforce;
/// * git reports zero commits, which means the query was wrong rather than that
///   the history is clean.
///
/// The scan covers every ref, not just `HEAD`. A banned trailer is still in this
/// repository's history as long as any ref reaches it, and restricting the scan
/// to the current branch would let a rewritten commit survive on a sibling branch
/// and read as clean.
fn authorship(name: &str, root: &Path, started: Instant, forbidden: &[String]) -> GateResult {
    let done = |outcome: Outcome, detail: String| GateResult {
        name: name.to_string(),
        outcome,
        duration: started.elapsed(),
        detail: Some(detail),
    };

    if forbidden.is_empty() {
        return done(
            Outcome::Unavailable,
            "[verification.attribution].forbidden_trailers is empty, so this gate has \
             nothing to forbid and cannot fail. Refusing to report a pass it did not \
             earn."
                .to_string(),
        );
    }

    let Some(git) = which_bin("git") else {
        return done(
            Outcome::Unavailable,
            "`git` is not installed, so commit history cannot be read".to_string(),
        );
    };

    // Run git with the runner's own environment scrubbed: `CARGO_*` describes
    // xtask's package, not the repository, and a stray one could redirect git at
    // a different object store.
    let git_out = |args: &[&str]| -> Result<String, String> {
        let mut cmd = Command::new(&git);
        cmd.args(args)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        scrub_cargo_env(&mut cmd);
        let out = cmd
            .output()
            .map_err(|e| format!("failed to run git {args:?}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    match git_out(&["rev-parse", "--is-inside-work-tree"]) {
        Ok(answer) if answer.trim() == "true" => {}
        Ok(answer) => {
            return done(
                Outcome::Unavailable,
                format!(
                    "{} is not inside a git work tree (git says {answer:?}), so there is \
                     no history to scan",
                    root.display()
                ),
            );
        }
        Err(e) => return done(Outcome::Unavailable, e),
    }

    match git_out(&["rev-parse", "--is-shallow-repository"]) {
        Ok(answer) if answer.trim() == "true" => {
            return done(
                Outcome::Unavailable,
                "this is a shallow clone, so the visible history is a truncation of the \
                 real one. Scanning it would report a pass over however many commits \
                 happen to be present. Clone with full history (`fetch-depth: 0` in CI)."
                    .to_string(),
            );
        }
        // Older git has no such flag. Not a reason to refuse: `git log` still
        // returns everything the clone actually has.
        Ok(_) | Err(_) => {}
    }

    // Record-separated, unit-separated. `--all` so every ref is covered; see the
    // note on why not just HEAD. The flag is attached to the value rather than
    // passed separately: a bare `--format` argument is read as a revision and git
    // rejects the whole query, which would have made this gate permanently
    // `Unavailable` while looking like a working one.
    let format = "--format=%x1e%H%x1f%B".to_string();
    let raw = match git_out(&["log", "--all", "--no-color", &format]) {
        Ok(out) => out,
        Err(e) => return done(Outcome::Unavailable, e),
    };

    // Normalised banned keys, paired with the spelling to name in a failure.
    let wanted: Vec<(String, &str)> = forbidden
        .iter()
        .map(|k| (normalise_key(k), k.as_str()))
        .collect();

    let mut scanned = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    for record in raw.split(RECORD_SEP) {
        let record = record.trim_start_matches('\n');
        if record.trim().is_empty() {
            continue;
        }
        let mut fields = record.splitn(3, FIELD_SEP);
        let (Some(sha), Some(message)) = (fields.next(), fields.next()) else {
            continue;
        };
        scanned += 1;
        let subject = redact_banned(message.lines().next().unwrap_or("").trim(), &wanted);

        for line in message.lines() {
            // A trailer is `Key: value`. Splitting on the *first* colon keeps the
            // value — which may itself contain colons, as an address does — out of
            // the comparison.
            let Some((key, _)) = line.split_once(':') else {
                continue;
            };
            let normalised = normalise_key(key);
            if let Some((_, declared)) = wanted.iter().find(|(w, _)| *w == normalised) {
                offenders.push(format!(
                    "  {}  {declared} trailer in {subject:?}",
                    &sha[..sha.len().min(8)]
                ));
            }
        }
    }

    if scanned == 0 {
        return done(
            Outcome::Unavailable,
            "git reported no commits, so the scan covered nothing. That is a broken \
             query, not a clean history."
                .to_string(),
        );
    }

    if offenders.is_empty() {
        return done(
            Outcome::Passed,
            format!(
                "{scanned} commit(s) scanned across all refs; no banned trailer in any \
                 message"
            ),
        );
    }

    done(
        Outcome::Failed,
        format!(
            "{} commit(s) carry a banned trailer:\n{}\n  Inspect one with \
             `git log -1 <id>` and amend the message; the value is not reproduced \
             here on purpose.",
            offenders.len(),
            offenders.join("\n")
        ),
    )
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
    /// `[meta]` is intentionally not deserialised, including its finding counts.
    /// Those counts are declared by the tracker *itself*, so a tracker that states
    /// its own expected contents can be edited to agree with whatever it currently
    /// holds — the defusal this gate exists to prevent. The counts compared against
    /// come from `theSix.toml`, which the tracker does not control.
    #[serde(default)]
    bug: Vec<BugEntry>,
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
    tracker: &str,
) -> GateResult {
    let done = |outcome: Outcome, detail: String| GateResult {
        name: name.to_string(),
        outcome,
        duration: started.elapsed(),
        detail: Some(detail),
    };

    let path = root.join(tracker);
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
    if let Some(want) = expected.blocking
        && want != actual_blocking
    {
        count_problems.push(format!(
            "theSix.toml expects {want} blocking finding(s), tracker holds {actual_blocking}"
        ));
    }
    if let Some(want) = expected.should_fix
        && want != actual_should_fix
    {
        count_problems.push(format!(
            "theSix.toml expects {want} should-fix finding(s), tracker holds {actual_should_fix}"
        ));
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
    use super::*;
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

    /// Tracker filename for fixtures. Matches what `theSix.toml` declares, so the
    /// gate reads the contract's path rather than a hardcoded one.
    const TRACKER: &str = "bugs.toml";

    fn check(body: &str) -> Outcome {
        // Expectations are derived from the fixture itself unless a test cares,
        // so a count assertion tests the count check rather than the fixture.
        let n_blocking = body.matches("blocks_merge = true").count();
        let n_soft = body.matches("blocks_merge = false").count();
        check_with(body, expected(n_blocking, n_soft))
    }

    fn check_with(body: &str, want: ExpectedFindings) -> Outcome {
        let dir = fixture(body);
        let r = merge_readiness(
            "merge_readiness",
            &dir,
            Instant::now(),
            Some(&want),
            TRACKER,
        );
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
            TRACKER,
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
            check(
                "[[bug]]\nid = \"B1\"\ntitle = \"t\"\nstatus = \"resolved\"\nblocks_merge = true\nrationale = \"guard added\"\n",
            ),
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
            TRACKER,
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
        let body = "[meta]\nfindings_blocking = 0\nfindings_should_fix = 1\n[[bug]]\nid = \"B1\"\ntitle = \"t\"\nstatus = \"open\"\nblocks_merge = false\n";
        assert_eq!(check_with(body, expected(3, 2)), Outcome::Failed);
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

    /// The gate runner must not hand `cargo run`'s environment to its children.
    ///
    /// `cargo xtask run <gate>` is `cargo run --package xtask --`, so cargo exports
    /// `CARGO_MANIFEST_DIR` and the `CARGO_PKG_*` family describing *xtask's own
    /// package*. Anything xtask spawns inherits them. `cargo machete` reads
    /// `CARGO_MANIFEST_DIR`, concluded it had been pointed at `xtask/`, and walked
    /// the wrong tree -- so `cargo xtask run machete` failed while
    /// `./target/debug/xtask run machete`, which has none of those variables,
    /// passed. The documented invocation was the broken one.
    ///
    /// The variables are read from this process rather than assigned, because
    /// `std::env::set_var` is unsafe in edition 2024 and this crate forbids unsafe
    /// code. That is not a limitation: `cargo test` sets exactly these for the test
    /// binary, so the test observes the real condition instead of a synthetic one.
    #[test]
    fn gate_children_do_not_inherit_cargos_own_package_environment() {
        assert!(
            std::env::var("CARGO_MANIFEST_DIR").is_ok(),
            "CARGO_MANIFEST_DIR must be set for this test to mean anything; cargo \
             sets it for test binaries, so being absent means the test is running \
             outside the situation it exists to check"
        );

        let mut cmd = Command::new("bash");
        scrub_cargo_env(&mut cmd);
        let out = cmd
            .arg("-c")
            .arg("echo \"[${CARGO_MANIFEST_DIR-unset}][${CARGO_PKG_NAME-unset}]\"")
            .output()
            .expect("bash must be runnable to test this");

        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "[unset][unset]",
            "a gate would inherit cargo's description of xtask's own package"
        );
    }

    /// The tracker path is read from the contract, not hardcoded in the gate.
    ///
    /// `[verification.merge_readiness].tracker` existed in the contract while the
    /// gate joined a literal `"bugs.toml"` onto the root -- a declared setting the
    /// code ignored. Asserting the default value would not catch that, so this
    /// rewrites the declared path and checks the loaded contract follows it.
    #[test]
    fn the_tracker_path_is_read_from_the_contract() {
        let real = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("theSix.toml");
        let text = std::fs::read_to_string(&real).expect("the contract must be readable");
        assert!(
            text.contains("tracker = \"bugs.toml\""),
            "the contract should declare tracker = \"bugs.toml\"; if it was renamed, this \
             fixture needs updating"
        );

        let dir = std::env::temp_dir().join(format!("thesix-tracker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let moved = dir.join("elsewhere.toml");
        std::fs::write(
            &moved,
            text.replace("tracker = \"bugs.toml\"", "tracker = \"elsewhere.toml\""),
        )
        .expect("write fixture");

        let contract = crate::contract::Contract::load(&moved).expect("fixture must load");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            contract.tracker, "elsewhere.toml",
            "the loaded contract ignored the declared tracker path"
        );
    }
}

// ---------------------------------------------------------------------------
// The authorship gate
// ---------------------------------------------------------------------------

#[cfg(test)]
mod authorship_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The keys `theSix.toml` declares. Read from the real contract rather than
    /// restated, so these tests break if the policy is edited instead of drifting
    /// silently against it.
    fn contract_keys() -> Vec<String> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("theSix.toml");
        let contract = crate::contract::Contract::load(&path).expect("the contract must load");
        contract.forbidden_trailers
    }

    /// The co-authorship trailer key, hand-written rather than read from the
    /// contract. Reading it from the contract would make the fixture and the
    /// matcher agree by construction, so a wrong key in `theSix.toml` would pass
    /// these tests while the gate matched nothing real.
    const BANNED_KEY: &str = "Co-Authored-By";

    /// A trailer built on that key, with a value that names nobody.
    ///
    /// The value is deliberately synthetic. The gate's claim is about the *key* —
    /// it stops at the first colon and never reads what follows — so reproducing a
    /// real attribution here would prove nothing extra while putting the very
    /// string the gate exists to keep out of this repository into its source. What
    /// has to be tested is "any value under this key is rejected", and
    /// `any_value_under_the_banned_key_is_rejected` tests exactly that.
    fn offending(value: &str) -> String {
        format!("{BANNED_KEY}: {value}")
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            // Before the subcommand: `git init --quiet -c k=v` is a usage error,
            // because `-c` is a git option and not one every subcommand takes.
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
                // Committer identity, set here rather than inherited. These
                // fixtures assert a property of git, so they must not depend on
                // the host's global config: `--author` sets the *author*, and git
                // still needs a committer, which it will otherwise take from
                // whatever `user.name`/`user.email` the machine happens to carry.
                "-c",
                "user.name=Test Committer",
                "-c",
                "user.email=committer@example.invalid",
            ])
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|e| {
                panic!(
                    "git {args:?} could not be spawned ({e}); these tests \
                 assert a property of git and cannot be skipped when git is absent"
                )
            });
        assert!(
            output.status.success(),
            // stderr is included because a bare "git commit failed" says nothing
            // about why, and these fixtures run in an environment the author does
            // not control. The first version discarded it, which turned a
            // runner-only failure into an unexplained one.
            "git {args:?} failed in fixture {}\nstderr: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    /// A fresh repository containing one commit with the given message.
    fn repo(message: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "thesix-auth-{}-{}-{n}",
            std::process::id(),
            message.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        git(&dir, &["init", "--quiet"]);
        git(
            &dir,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                message,
                "--author",
                "Test Author <author@example.invalid>",
            ],
        );
        dir
    }

    fn check(dir: &Path) -> GateResult {
        authorship("authorship", dir, Instant::now(), &contract_keys())
    }

    fn check_repo(message: &str) -> Outcome {
        let dir = repo(message);
        let r = check(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        r.outcome
    }

    /// The gate exists to fail. Every other test in this module is meaningless if
    /// this one does not hold, because a gate that cannot reject the trailer it
    /// was written for protects nothing.
    #[test]
    fn a_co_authorship_trailer_fails_the_gate() {
        assert_eq!(
            check_repo(&offending("Some Assistant <assistant@example.invalid>")),
            Outcome::Failed
        );
    }

    /// The key is the rule and the value is not, so this holds for every value —
    /// which is why the fixtures above need not name a real one. A gate that
    /// matched on the value would pass this suite while the real attribution came
    /// back tomorrow under a new model name.
    #[test]
    fn any_value_under_the_banned_key_is_rejected() {
        for value in [
            "A Person <person@example.invalid>",
            "Some Assistant <assistant@example.invalid>",
            "Some Assistant (2026-01-01) <noreply@example.invalid>",
            "<>",
            "",
        ] {
            assert_eq!(
                check_repo(&offending(value)),
                Outcome::Failed,
                "missed the value {value:?}"
            );
        }
    }

    /// And the failure must be locatable: reporting "something is wrong" without
    /// naming the commit would make a 50-commit history expensive to clean.
    #[test]
    fn the_failure_names_the_offending_commit() {
        let dir = repo(&format!(
            "fix: a real subject\n\nBody.\n\n{}\n",
            offending("Some Assistant <assistant@example.invalid>")
        ));
        let r = check(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.outcome, Outcome::Failed);
        let detail = r.detail.expect("a failing gate must explain itself");
        assert!(
            detail.contains("fix: a real subject"),
            "the failure must name the commit by subject: {detail}"
        );
        assert!(
            !detail.contains("assistant@example.invalid"),
            "the failure must not reproduce the attribution it is reporting: {detail}"
        );
    }

    /// The subject is printed so the commit is identifiable, which means a commit whose
    /// *entire message* is the banned line would have the gate reprint it. That is the
    /// shape a tool produces when it appends the trailer as its own commit, so it is
    /// not hypothetical — and it would undo the gate by reintroducing the string into
    /// the one artefact that gets pasted into issues and logs.
    #[test]
    fn a_message_that_is_only_the_trailer_does_not_reprint_it() {
        let dir = repo(&offending("Some Assistant <assistant@example.invalid>"));
        let r = check(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.outcome, Outcome::Failed);
        let detail = r.detail.expect("a failing gate must explain itself");
        for leak in ["Some Assistant", "assistant@example.invalid"] {
            assert!(
                !detail.contains(leak),
                "the gate reprinted {leak:?} from the commit it was reporting: {detail}"
            );
        }
        assert!(
            detail.contains("withheld"),
            "a redacted subject should say so rather than look like a truncated subject: \
         {detail}"
        );
    }

    #[test]
    fn a_plain_commit_passes() {
        assert_eq!(check_repo("fix: an ordinary message"), Outcome::Passed);
    }

    /// A message with a body and an unrelated trailer is still a pass. Without
    /// this, a gate that flagged any trailer at all would be indistinguishable
    /// from one that enforces the policy.
    #[test]
    fn an_unrelated_trailer_is_not_a_violation() {
        assert_eq!(
            check_repo(
                "fix: signed off\n\nBody line.\n\nReviewed-by: A Person\nSigned-off-by: Another Person"
            ),
            Outcome::Passed
        );
    }

    /// The gate is not scoped to the trailer block.
    ///
    /// Git's own trailer rules only recognise the final paragraph of a message,
    /// so a co-author line separated from the subject by a blank line is not a
    /// trailer by git's definition — while being exactly as much of an
    /// attribution. A gate that trusted `git interpret-trailers` semantics here
    /// would be defeated by inserting one blank line.
    #[test]
    fn a_disowned_trailer_still_fails() {
        assert_eq!(
            check_repo(&format!(
                "fix: subject\n\nBody first.\n\n{}\n",
                offending("Some Assistant <assistant@example.invalid>")
            )),
            Outcome::Failed
        );
    }

    /// Spelling variants are one rule, not four.
    #[test]
    fn case_and_separator_variants_are_all_caught() {
        for variant in [
            "co-authored-by: Someone <s@example.invalid>",
            "Co-authored-by: Someone <s@example.invalid>",
            "CoAuthoredBy: Someone <s@example.invalid>",
            "Co_Authored_By: Someone <s@example.invalid>",
            "Co-Author-by: Someone <s@example.invalid>",
        ] {
            assert_eq!(
                check_repo(variant),
                Outcome::Failed,
                "missed the variant {variant:?}"
            );
        }
    }

    /// A colon in the value must not shift the comparison. Addresses contain
    /// colons and would otherwise turn into a key that matches nothing.
    #[test]
    fn a_value_containing_a_colon_is_still_caught() {
        assert_eq!(
            check_repo("Co-Authored-By: a:b:c <s@example.invalid:9000>"),
            Outcome::Failed
        );
    }

    /// An empty policy is a gate that cannot fail. `Unavailable`, not `Passed`.
    #[test]
    fn an_empty_policy_is_unavailable_not_passed() {
        let dir = repo("fix: clean");
        let r = authorship("authorship", &dir, Instant::now(), &[]);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.outcome, Outcome::Unavailable);
    }

    /// A directory that is not a repository has no history to scan, which is not
    /// the same as having a clean one.
    #[test]
    fn a_non_repository_is_unavailable_not_passed() {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("thesix-norepo-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let r = check(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(r.outcome, Outcome::Unavailable);
    }

    /// A shallow clone is the failure mode this gate would otherwise have shipped
    /// with. GitHub's default checkout is depth 1, so without this check CI would
    /// have reported a pass over the single tip commit while the banned trailer
    /// sat in every commit beneath it.
    #[test]
    fn a_shallow_clone_is_unavailable_not_passed() {
        let source = repo("fix: clean");
        let dir = std::env::temp_dir().join(format!(
            "thesix-shallow-{}-{}",
            std::process::id(),
            source.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        git(
            std::path::Path::new("/"),
            &[
                "clone",
                "--quiet",
                "--depth",
                "1",
                "--no-local",
                source.to_str().expect("utf-8 temp path"),
                dir.to_str().expect("utf-8 temp path"),
            ],
        );
        let r = check(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&source);
        assert_eq!(r.outcome, Outcome::Unavailable);
    }

    /// The scan covers every ref, not just `HEAD`. A rewritten commit that is still
    /// reachable from a sibling branch is still in this repository's history.
    #[test]
    fn a_trailer_reachable_only_from_another_branch_still_fails() {
        let dir = repo("fix: clean");
        git(&dir, &["branch", "sibling"]);
        let message = offending("Some Assistant <assistant@example.invalid>");
        git(
            &dir,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                &message,
                "--author",
                "Test Author <author@example.invalid>",
            ],
        );
        // Move HEAD back to the clean commit, so only `sibling` reaches the
        // offending one. A HEAD-only scan would now report a clean history.
        let head = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "main"])
                .current_dir(&dir)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .trim()
        .to_string();
        git(&dir, &["checkout", "--quiet", "--detach", &head]);
        let r = check(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.outcome,
            Outcome::Failed,
            "the offending commit is only reachable from `sibling`; a HEAD-only scan \
             would have passed this"
        );
    }

    /// The policy is read from the contract, not hardcoded in the gate.
    #[test]
    fn the_policy_comes_from_the_contract() {
        let keys = contract_keys();
        assert!(
            !keys.is_empty(),
            "[verification.attribution].forbidden_trailers is empty, so the gate has \
             nothing to forbid"
        );
        let normalised: Vec<String> = keys.iter().map(|k| super::normalise_key(k)).collect();
        assert!(
            normalised.iter().any(|k| k == "coauthoredby"),
            "the contract must forbid the co-authorship trailer. Keys declared: {keys:?}"
        );
    }

    /// The contract declares the trailer *key*, and this gate is why that matters:
    /// if the key it compares against were assembled from the same source as the
    /// fixture, both would move together and a wrong matcher would still pass.
    /// So the matcher is checked directly against hand-written spellings.
    #[test]
    fn normalisation_collapses_separators_and_case() {
        assert_eq!(super::normalise_key("Co-Authored-By"), "coauthoredby");
        assert_eq!(super::normalise_key("co-authored-by"), "coauthoredby");
        assert_eq!(super::normalise_key("CoAuthoredBy"), "coauthoredby");
        assert_eq!(super::normalise_key("  Co_Authored_By  "), "coauthoredby");
        assert_ne!(
            super::normalise_key("Co-Author-by"),
            super::normalise_key("Co-Authored-By"),
            "a different spelling is a different key; conflating them would widen the \
             rule beyond what the contract declares"
        );
    }
}
