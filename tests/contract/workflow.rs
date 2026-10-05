//! Cross-checks between `theSix.toml` and `.github/workflows/ci.yml`.
//!
//! The previous revision of this repository carried a comment in `ci.yml`
//! claiming that "`cargo xtask contract` fails if this workflow and the contract
//! disagree about which gates are mandatory". No code read `ci.yml`. The claim
//! was load-bearing prose describing a check that did not exist, which is the
//! exact failure this crate exists to prevent — so the checks live here now.
//!
//! Four directions are covered, because each one alone is defeatable:
//!
//! * every gate the contract declares is executed by some CI job;
//! * every entry in a job's `covers` names a gate that exists;
//! * every job in the workflow is declared in the contract (an undeclared extra
//!   job would otherwise verify nothing and still report green);
//! * every declared check name matches the name the workflow actually reports, so
//!   a renamed job fails here instead of silently ceasing to be required.
//!
//! Plus the two structural properties that make branch protection on a single
//! aggregated check sound rather than decorative, and the `bash -n` lint that
//! keeps a malformed `run:` block from being a CI step that fails before it runs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

/// The contract, embedded at compile time so a missing file is a compile error.
const CONTRACT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/theSix.toml"));

const AGGREGATOR: &str = "verify";

fn workflow_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/ci.yml")
}

// ---------------------------------------------------------------------------
// Contract side
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ContractRoot {
    verification: ContractVerification,
}

#[derive(Debug, Deserialize)]
struct ContractVerification {
    gate: Vec<ContractGate>,
    #[serde(default)]
    ci_job: Vec<ContractJob>,
}

#[derive(Debug, Deserialize)]
struct ContractGate {
    name: String,
    /// Empty for gates that are not test gates; used only to match a `run:`
    /// block against the gate it claims to cover.
    #[serde(default)]
    targets: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct ContractJob {
    job: String,
    check_name: String,
    #[serde(default)]
    covers: Vec<String>,
    /// Declared advisory so "non-blocking" is a decision in the contract rather
    /// than an accident of `continue-on-error` in the workflow.
    #[serde(default)]
    advisory: bool,
    #[serde(default)]
    aggregates_into: Option<String>,
}

fn contract_jobs() -> Vec<ContractJob> {
    toml::from_str::<ContractRoot>(CONTRACT)
        .expect("theSix.toml must parse; if it does not, the contract gate has no meaning")
        .verification
        .ci_job
}

fn contract_gates() -> BTreeSet<String> {
    toml::from_str::<ContractRoot>(CONTRACT)
        .expect("theSix.toml must parse")
        .verification
        .gate
        .into_iter()
        .map(|g| g.name)
        .collect()
}

/// Whether this job's matrix enumerates `gate` under any key.
fn matrix_runs_gate(matrix: &BTreeMap<String, toml::Value>, gate: &str) -> bool {
    matrix.iter().any(|(_, value)| match value {
        toml::Value::Array(items) => items.iter().any(|item| item.as_str() == Some(gate)),
        toml::Value::String(s) => s == gate,
        _ => false,
    })
}

/// Gate name -> the targets it runs, so an invocation can be matched against it.
fn gate_targets() -> BTreeMap<String, Vec<String>> {
    toml::from_str::<ContractRoot>(CONTRACT)
        .expect("theSix.toml must parse")
        .verification
        .gate
        .into_iter()
        .map(|g| (g.name, g.targets))
        .collect()
}

// ---------------------------------------------------------------------------
// Workflow side
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WorkflowJob {
    /// The name GitHub reports. Falls back to the job key when absent, which is
    /// what GitHub itself does.
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "continue-on-error")]
    continue_on_error: bool,
    #[serde(default)]
    needs: JobNeeds,
    #[serde(default)]
    steps: Vec<WorkflowStep>,
    /// A `needs` job without this is *skipped* when a dependency fails, and
    /// GitHub scores a skipped required check as passing — which would make the
    /// aggregator report success by never running.
    #[serde(default, rename = "if")]
    if_: Option<String>,
    /// The `strategy.matrix` block, read for the same reason `steps` is: a
    /// matrix job invokes `cargo xtask run ${{ matrix.layer }}`, so the gate name
    /// appears in the matrix values and not in the `run:` text. Matching only
    /// `run:` would reject the one job in this workflow that runs ten gates.
    #[serde(default)]
    strategy: WorkflowStrategy,
}

#[derive(Debug, Default, Deserialize)]
struct WorkflowStrategy {
    #[serde(default)]
    matrix: WorkflowMatrix,
}

#[derive(Debug, Default, Deserialize)]
struct WorkflowMatrix {
    /// Every scalar under `matrix`, flattened. A gate can live at `matrix.layer`
    /// or `matrix.gate` depending on the job, and this does not care which.
    #[serde(flatten)]
    values: BTreeMap<String, toml::Value>,
}

impl WorkflowJob {
    fn reported_name<'a>(&'a self, key: &'a str) -> &'a str {
        self.name.as_deref().unwrap_or(key)
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
enum JobNeeds {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl JobNeeds {
    fn list(&self) -> Vec<&str> {
        match self {
            JobNeeds::None => Vec::new(),
            JobNeeds::One(s) => vec![s.as_str()],
            JobNeeds::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowStep {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    run: Option<String>,
}

#[derive(Debug)]
struct Workflow {
    jobs: BTreeMap<String, WorkflowJob>,
    /// Whether `pull_request` is unfiltered. `None` means the filter was removed,
    /// which is what makes a stacked PR run CI at all.
    pull_request_unfiltered: bool,
}

fn parse_workflow(source: &str) -> Workflow {
    let doc = serde_yaml::from_str::<serde_yaml::Value>(source).expect(
        "ci.yml must be valid YAML; the contract cannot be checked against a file it cannot read",
    );

    // `on:` is the value `true` under YAML 1.1's boolean rules, which is a
    // long-standing trap for anything reading a GitHub workflow. Both spellings
    // are accepted so this does not depend on the parser's schema version.
    let trigger = ["on", "true"]
        .iter()
        .find_map(|k| doc.get(serde_yaml::Value::String((*k).to_string())))
        .and_then(|v| v.as_mapping());

    let pull_request_unfiltered = trigger
        .and_then(|m| m.get(serde_yaml::Value::String("pull_request".to_string())))
        .is_some_and(|v| v.is_null() || v.as_mapping().is_none());

    let jobs_value = doc
        .get(serde_yaml::Value::String("jobs".to_string()))
        .expect("ci.yml must declare jobs");

    let jobs = serde_yaml::from_value::<BTreeMap<String, WorkflowJob>>(jobs_value.clone())
        .expect("each job must deserialise; an unexpected shape here is a workflow defect");

    Workflow {
        jobs,
        pull_request_unfiltered,
    }
}

fn workflow() -> Workflow {
    let source = std::fs::read_to_string(workflow_path()).expect("ci.yml must be readable");
    parse_workflow(&source)
}

// ---------------------------------------------------------------------------
// Coverage, both directions
// ---------------------------------------------------------------------------

#[test]
fn every_declared_gate_is_executed_by_a_ci_job() {
    let gates = contract_gates();
    let jobs = contract_jobs();
    let covered: BTreeSet<String> = jobs.iter().flat_map(|j| j.covers.clone()).collect();

    let missing: Vec<&String> = gates.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "these gates are declared mandatory in theSix.toml but no CI job runs them: {missing:?}.\n\
         A gate only the maintainer's machine executes is not a gate. Add a job and a \
         [[verification.ci_job]] entry that covers it."
    );
}

/// A `covers` entry is a claim about a `run:` block, not a label.
///
/// This assertion is the reason the previous one was not sufficient. The `contract`
/// job listed `contract` in `covers` for its entire life while executing
/// `cargo xtask contract`, which validates and prints, instead of
/// `cargo xtask run contract`, which is the gate. Both strings contain "contract",
/// so a name comparison passed and all 56 assertions in `tests/contract` compiled,
/// linted and never ran. A coverage table that records an intention instead of an
/// invocation is worse than no coverage table, because it is the thing reviewers
/// read.
///
/// Two invocations count, matching the two shapes the workflow uses: the runner
/// (`cargo xtask run <gate>`), or nextest naming the gate's own targets.
#[test]
fn a_covering_job_actually_invokes_the_gate_it_claims_to_cover() {
    let gates = gate_targets();
    let wf = workflow();
    let mut failures = Vec::new();

    for declaration in contract_jobs() {
        // `covers` lives on the contract side; the steps live on the workflow side.
        // They meet on the job key. A declaration naming a job the workflow does
        // not have is a different defect and belongs to the other assertion.
        let Some(job) = wf.jobs.get(&declaration.job) else {
            continue;
        };
        let scripts: Vec<String> = job.steps.iter().filter_map(|s| s.run.clone()).collect();
        for gate in &declaration.covers {
            let Some(targets) = gates.get(gate) else {
                continue; // named and covered; `every_covered_entry_names_a_real_gate` owns that
            };
            // `cargo xtask run <gate>` with the gate named literally, or the
            // matrix form `cargo xtask run ${{ matrix.<key> }}` where this job's
            // matrix enumerates the gate. The second form has to be resolved
            // against the matrix values or the only job running ten gates fails.
            let via_runner = scripts.iter().any(|s| {
                let words: Vec<&str> = s.split_whitespace().collect();
                words
                    .windows(3)
                    .any(|w| w[0] == "xtask" && w[1] == "run" && w[2] == *gate)
                    || words.windows(3).any(|w| {
                        w[0] == "xtask"
                            && w[1] == "run"
                            && w[2].starts_with("${{")
                            && matrix_runs_gate(&job.strategy.matrix.values, gate)
                    })
            });
            let via_nextest = !targets.is_empty()
                && targets.iter().all(|t| {
                    let flag: Vec<&str> = if t == "lib" {
                        vec!["--lib"]
                    } else {
                        vec!["--test", t.as_str()]
                    };
                    scripts.iter().any(|s| {
                        s.split_whitespace()
                            .collect::<Vec<_>>()
                            .windows(flag.len())
                            .any(|w| w == flag)
                    })
                });
            if !via_runner && !via_nextest {
                failures.push(format!(
                    "job {:?} covers gate {gate:?} but no step in it runs it.\n\
                     Either invoke it (`cargo xtask run {gate}`) or name its targets \
                     in a nextest step, or drop it from `covers` -- a coverage entry \
                     naming a gate nobody executes is a claim, not coverage.\n\
                     targets: {targets:?}",
                    declaration.job
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_covered_entry_names_a_real_gate() {
    let gates = contract_gates();
    let jobs = contract_jobs();
    let bogus: Vec<&String> = jobs
        .iter()
        .flat_map(|j| j.covers.iter())
        .filter(|g| !gates.contains(*g))
        .collect();

    assert!(
        bogus.is_empty(),
        "[[verification.ci_job]].covers names gates that do not exist: {bogus:?}. \
         A typo here reads as coverage while executing nothing."
    );
}

#[test]
fn every_ci_job_is_declared_in_the_contract() {
    let jobs = contract_jobs();
    let declared: BTreeSet<&str> = jobs.iter().map(|j| j.job.as_str()).collect();
    let wf = workflow();
    let actual: BTreeSet<&str> = wf.jobs.keys().map(String::as_str).collect();

    let undeclared: Vec<&str> = actual.difference(&declared).copied().collect();
    assert!(
        undeclared.is_empty(),
        "ci.yml defines jobs the contract does not declare: {undeclared:?}. \
         An undeclared job verifies nothing the contract knows about, and still reports green."
    );

    let missing: Vec<&str> = declared.difference(&actual).copied().collect();
    assert!(
        missing.is_empty(),
        "[[verification.ci_job]] describes jobs ci.yml does not define: {missing:?}."
    );
}

#[test]
fn declared_check_names_match_what_the_workflow_reports() {
    let wf = workflow();
    let jobs = contract_jobs();
    let mut mismatches = Vec::new();

    for declared in &jobs {
        let Some(job) = wf.jobs.get(&declared.job) else {
            continue; // absence is the previous test's failure to report
        };
        let actual = job.reported_name(&declared.job);
        if actual != declared.check_name {
            mismatches.push(format!(
                "{}: contract says {:?}, workflow reports {actual:?}",
                declared.job, declared.check_name
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "declared check names disagree with the workflow: {mismatches:#?}.\n\
         Branch protection matches on the reported name, so a job renamed in one \
         place and not the other stops being required without ever failing."
    );
}

#[test]
fn advisory_status_is_declared_and_agrees_with_the_workflow() {
    let wf = workflow();
    let jobs = contract_jobs();
    for declared in &jobs {
        let Some(job) = wf.jobs.get(&declared.job) else {
            continue;
        };
        assert_eq!(
            declared.advisory, job.continue_on_error,
            "job `{}`: theSix.toml says advisory={} but ci.yml says \
             continue-on-error={}. Advisory must be a declared decision, because \
             `continue-on-error` reporting a syntax error as an allowed failure \
             is how a job that cannot run looks advisory rather than broken.",
            declared.job, declared.advisory, job.continue_on_error
        );
    }
}

#[test]
fn verification_runs_on_every_pull_request() {
    assert!(
        workflow().pull_request_unfiltered,
        "ci.yml scopes `pull_request` to a branch filter, so a PR based on another \
         feature branch never runs CI. This repo's PRs are stacked by design, so \
         that filter excluded exactly the branches most in need of verification."
    );
}

// ---------------------------------------------------------------------------
// The aggregator that branch protection will require
// ---------------------------------------------------------------------------

#[test]
fn exactly_one_job_is_the_declared_aggregator() {
    let jobs = contract_jobs();
    let aggregators: Vec<&ContractJob> = jobs
        .iter()
        .filter(|j| j.aggregates_into.is_some())
        .collect();

    assert_eq!(
        aggregators.len(),
        1,
        "exactly one job must declare `aggregates_into`, so branch protection has \
         one required check name to match. Found {}.",
        aggregators.len()
    );
    assert_eq!(
        aggregators[0].aggregates_into.as_deref(),
        Some(AGGREGATOR),
        "the declaring job must be the aggregator itself"
    );
}

#[test]
fn the_aggregator_runs_even_when_a_dependency_fails() {
    let wf = workflow();
    let aggregator = wf
        .jobs
        .get(AGGREGATOR)
        .unwrap_or_else(|| panic!("ci.yml must define the `{AGGREGATOR}` job"));

    assert_eq!(
        aggregator.if_.as_deref(),
        Some("always()"),
        "the aggregator needs `if: always()`. Without it a failed dependency makes \
         the aggregator *skipped*, and GitHub scores a skipped required check as \
         passing — so the one job branch protection would require would report \
         success precisely when something broke."
    );
}

#[test]
fn every_non_advisory_job_is_wired_into_the_aggregator() {
    let wf = workflow();
    let aggregator = wf
        .jobs
        .get(AGGREGATOR)
        .unwrap_or_else(|| panic!("ci.yml must define the `{AGGREGATOR}` job"));
    let needed: BTreeSet<&str> = aggregator.needs.list().into_iter().collect();

    let unwired: Vec<&str> = wf
        .jobs
        .iter()
        .filter(|(key, job)| !job.continue_on_error && key.as_str() != AGGREGATOR)
        .map(|(key, _)| key.as_str())
        .filter(|k| !needed.contains(k))
        .collect();

    assert!(
        unwired.is_empty(),
        "these jobs are not in `{AGGREGATOR}`'s needs: {unwired:?}. A job missing \
         from `needs` runs and reports its own result, but the aggregated check \
         stays green while it does — which is the gap the aggregator exists to close."
    );
}

#[test]
fn the_aggregator_needs_only_jobs_that_exist() {
    let wf = workflow();
    let aggregator = wf
        .jobs
        .get(AGGREGATOR)
        .unwrap_or_else(|| panic!("ci.yml must define the `{AGGREGATOR}` job"));

    let dangling: Vec<&str> = aggregator
        .needs
        .list()
        .into_iter()
        .filter(|n| !wf.jobs.contains_key(*n))
        .collect();

    assert!(
        dangling.is_empty(),
        "`{AGGREGATOR}` needs jobs that do not exist: {dangling:?}. GitHub fails a \
         workflow whose `needs` names a missing job, so this takes the whole run down."
    );
}

// ---------------------------------------------------------------------------
// `bash -n` over every run: block
// ---------------------------------------------------------------------------

/// Replace `${{ ... }}` with an identifier.
///
/// GitHub substitutes these before bash sees them. Left in place they are a
/// *syntax* error rather than a no-op: bash reads `${` as the start of a
/// parameter expansion, so `echo "${{ matrix.layer }}"` does not print the value,
/// it fails to parse. The lint substitutes them so it reports real shell defects
/// instead of expression syntax.
fn strip_expressions(script: &str) -> String {
    let mut out = String::with_capacity(script.len());
    let mut rest = script;
    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        match rest[start..].find("}}") {
            Some(end) => {
                out.push_str("EXPR");
                rest = &rest[start + end + 2..];
            }
            None => {
                // Unterminated expression: hand it to bash and let bash complain.
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `bash -n` over one script, returning the parser's complaint.
///
/// Reads the script on stdin rather than via a temp file: fewer moving parts,
/// and no path to collide on. A missing `bash` is a panic, never a skip — the
/// repository's convention is that an absent tool must not read as a pass.
fn bash_syntax_error(script: &str) -> Option<String> {
    let mut child = Command::new("bash")
        .arg("-n")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            panic!("could not spawn bash ({e}); the workflow lint cannot be skipped silently")
        });

    // Scoped so stdin closes before the wait: otherwise bash blocks on a
    // half-read script while we block on its stderr.
    {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        stdin
            .write_all(script.as_bytes())
            .expect("writing the script to bash");
    }

    let out = child.wait_with_output().expect("waiting on bash");
    if out.status.success() {
        None
    } else {
        Some(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Lint every `run:` block, returning one message per offending step.
fn lint_run_blocks(wf: &Workflow) -> Vec<String> {
    let mut failures = Vec::new();
    for (job_key, job) in &wf.jobs {
        for step in &job.steps {
            let Some(script) = &step.run else { continue };
            let label = step
                .name
                .clone()
                .unwrap_or_else(|| "(unnamed step)".to_string());
            if let Some(err) = bash_syntax_error(&strip_expressions(script)) {
                failures.push(format!("{job_key} / {label}: {err}"));
            }
        }
    }
    failures
}

#[test]
fn every_run_block_is_valid_bash() {
    let wf = workflow();
    let blocks: usize = wf
        .jobs
        .values()
        .map(|j| j.steps.iter().filter(|s| s.run.is_some()).count())
        .sum();

    let failures = lint_run_blocks(&wf);
    assert!(
        failures.is_empty(),
        "{} of {blocks} `run:` blocks in ci.yml are not valid bash:\n  {}\n\n\
         GitHub runs a multiline `run:` as one script, so a syntax error fails the \
         step before it executes anything. Under `continue-on-error` that surfaces \
         as an allowed advisory failure, so the job looks green while doing nothing.",
        failures.len(),
        failures.join("\n  ")
    );
    assert!(
        blocks > 0,
        "no run: blocks found — the lint is not reading the file"
    );
}

/// The lint's own regression test.
///
/// Without this, "every run: block is valid bash" is satisfied by a lint that
/// reports nothing — which is precisely the shape of the defect it replaces: a
/// check that cannot fail. This feeds the *same* extraction and lint path the
/// test above uses a malformed workflow through B7's exact defect and requires
/// it to be caught.
#[test]
fn the_workflow_lint_rejects_the_defect_it_exists_to_catch() {
    let broken = r#"
name: CI
on:
  push:
    branches: [main]
jobs:
  fuzz:
    name: Fuzz (nightly, advisory)
    runs-on: ubuntu-latest
    continue-on-error: true
    steps:
      - uses: actions/checkout@v4
      - name: Smoke-run each target
        run: |
          status=0
          for t in $(cargo +nightly fuzz list); do
            echo "::group::fuzz $t"
            cargo +nightly fuzz run "$t" || status=1
          done
          if [ "$status" -ne 0 ]; then
            exit 1
          fi
          echo "::endgroup::"
          done
"#;

    let wf = parse_workflow(broken);
    let failures = lint_run_blocks(&wf);

    assert!(
        !failures.is_empty(),
        "the workflow lint accepted a `run:` block with an unmatched `done`. This is \
         B7 exactly: the step fails to parse, so the smoke loop never runs, and \
         `continue-on-error` reports it as an allowed advisory failure. A lint that \
         cannot reject this is as vacuous as the comment it replaced."
    );
    assert!(
        failures[0].contains("fuzz"),
        "the failure must name the offending job so it can be found: {failures:?}"
    );
}
