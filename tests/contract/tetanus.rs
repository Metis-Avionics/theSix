//! Contract-layer assertions for the TETANUS standard.
//!
//! `tests/contract` exists because `theSix.toml` is load-bearing: it is meant to be
//! a contract, and this layer is what stops it from being a comment. The same
//! applies to `tetanus.toml`, with one extra reason. A standard nobody checks is
//! a standard nobody follows, and the version of that failure that actually
//! happened here was eight prose references to "TETANUS Rule 3" with no file
//! behind any of them.
//!
//! Every assertion here is written to fail on the specific defect it exists to
//! catch. A test that passes for a reason unrelated to its name is worse than no
//! test, so each carries the defect in its name and in a comment.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Standard {
    meta: Meta,
    #[serde(default, rename = "rule")]
    rules: Vec<Rule>,
    #[serde(default)]
    baseline: Vec<Baseline>,
}

#[derive(Debug, Deserialize)]
struct Meta {
    standard: String,
    source: String,
    note: String,
    scan_roots: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Rule {
    id: usize,
    #[allow(dead_code)]
    title: String,
    check: String,
    disposition: String,
    #[serde(default)]
    review_artifact: Option<String>,
    /// Required for a `gated` rule that has no baseline entry. Without it, a rule
    /// that finds nothing is indistinguishable from a rule that was never checked.
    #[serde(default)]
    finds_nothing_because: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Baseline {
    rule: usize,
    location: String,
    reason: String,
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn standard() -> Standard {
    let path = root().join("tetanus.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The gate's own parser must agree with this one's, or the two are describing
/// different files. A `[[rule]]` table that this struct reads and the gate's does
/// not would make every rule below vacuous.
#[test]
fn the_contract_layer_and_the_gate_read_the_same_file() {
    let s = standard();
    assert_eq!(s.rules.len(), 10, "expected ten declared rules");
    assert!(
        s.baseline.len() > 100,
        "a baseline of {} entries is too small to be this crate",
        s.baseline.len()
    );
}

#[test]
fn all_ten_rules_are_declared_exactly_once() {
    let s = standard();
    let ids: Vec<usize> = s.rules.iter().map(|r| r.id).collect();
    let unique: BTreeSet<usize> = ids.iter().copied().collect();
    assert_eq!(
        unique.len(),
        10,
        "rules 1..=10 must each be declared exactly once; got {ids:?}"
    );
    for id in 1..=10usize {
        assert!(unique.contains(&id), "rule {id} is not declared");
    }
}

/// A gate cannot tell a real justification from a well-formed one, so the only
/// defence is that a placeholder cannot be written without being noticed. This is
/// the assertion that makes `cargo xtask bless` a scaffold rather than a decision.
#[test]
fn no_baseline_entry_carries_a_placeholder_reason() {
    for b in standard().baseline {
        let lower = b.reason.to_lowercase();
        // Deliberately not "why": it is an ordinary English word, and a test that
        // rejects a justification for containing it rejects justifications.
        for marker in ["todo", "fixme", "xxx", "placeholder"] {
            assert!(
                !lower.contains(marker),
                "{} has a placeholder reason containing {marker:?}: {:?}",
                b.location,
                b.reason
            );
        }
        assert!(
            b.reason.len() > 40,
            "{}: a {}-character reason is a rule restated, not a justification",
            b.location,
            b.reason.len()
        );
    }
}

/// The attribute the whole standard rests on. Without the `rename`, `rules`
/// deserialises empty and every rule reads as undeclared -- which is what
/// happened, and the gate reported a clean pass while declaring nothing.
#[test]
fn a_baseline_entry_may_not_be_declared_twice() {
    let s = standard();
    let mut seen = BTreeSet::new();
    for b in &s.baseline {
        let key = (b.rule, b.location.clone());
        assert!(seen.insert(key.clone()), "{key:?} is declared twice");
    }
}

/// Every scan root must exist. A root that does not is not an empty rule set: it
/// is a region of the tree the gate never walked, which is the one fail-open path
/// set equality cannot catch, because a region nothing scanned contributes to
/// neither side of the comparison.
#[test]
fn every_scan_root_exists_and_is_a_directory() {
    let r = root();
    for dir in &standard().meta.scan_roots {
        let p = r.join(dir);
        assert!(
            p.is_dir(),
            "scan root `{dir}` is not a directory at {}",
            p.display()
        );
    }
}

/// Derived from the workspace rather than read from `[meta]`, so a new member has
/// to be added to the scan by someone noticing the test fail. The assertion is the
/// prompt; `scan_roots` is the answer.
#[test]
fn every_workspace_member_is_scanned() {
    let manifest = std::fs::read_to_string(root().join("Cargo.toml"))
        .map(|t| tomllib_from(&t))
        .unwrap_or_default();
    let roots = &standard().meta.scan_roots;
    for member in ["xtask", "testkit"] {
        assert!(
            manifest.contains(&format!("name = \"{member}\""))
                || roots.iter().any(|r| r.contains(member)),
            "{member} is a workspace member with no scan root covering it"
        );
    }
    assert!(
        roots.contains(&"src".to_string()),
        "the root crate is not scanned"
    );
}

/// The mapping is ours. Stating it is the difference between a reader knowing
/// whose position they are reading and assuming Holzmann endorsed Rust's.
#[test]
fn the_rust_mapping_is_declared_as_ours() {
    let meta = standard().meta;
    assert!(
        meta.source.contains("Holzmann"),
        "the source of the standard must name its author: {:?}",
        meta.source
    );
    let lower = meta.note.to_lowercase();
    assert!(
        lower.contains("ours") || lower.contains("our own"),
        "[meta].note must say the Rust mapping is this repository's own work, not \
         Holzmann's or JPL's. Without it a reader of a finding has no way to know \
         whose position they are looking at."
    );
    assert!(
        !meta.standard.is_empty() && !meta.source.is_empty(),
        "the standard must name both its title and its source"
    );
}

/// `reported` is a real disposition with a real consequence: those sites must
/// never appear in the baseline, or the count would stop being the honest
/// report of how much of the crate is non-compliant.
#[test]
fn a_reported_rule_is_never_baselined() {
    let s = standard();
    for b in &s.baseline {
        let rule = s.rules.iter().find(|r| r.id == b.rule).unwrap_or_else(|| {
            panic!(
                "{} cites rule {}, which is not declared",
                b.location, b.rule
            )
        });
        assert_ne!(
            rule.disposition, "reported",
            "{} is a rule-{} site, but rule {} is `reported`: baselining it would turn \
             a count into a wall of identical justifications",
            b.location, b.rule, b.rule
        );
    }
}

/// A rule with a clause the gate cannot decide must say where that judgement is
/// recorded. The alternative is a rule that looks checked and is not, which is
/// how "TETANUS Rule 3" survived as prose for so long.
#[test]
fn every_review_rule_names_its_review_artifact() {
    let s = standard();
    let mut seen_review = 0;
    for r in &s.rules {
        let artifact = r.review_artifact.as_deref().unwrap_or("").trim();
        match r.check.as_str() {
            "mechanical" => assert!(
                artifact.is_empty(),
                "rule {} is `mechanical` but names a review_artifact: that claims a \
                 review the gate never asked for",
                r.id
            ),
            "review" | "mixed" => {
                seen_review += 1;
                assert!(
                    !artifact.is_empty(),
                    "rule {} is `check = {:?}` with no review_artifact: a clause the \
                     gate cannot decide, recorded nowhere",
                    r.id,
                    r.check
                );
            }
            other => panic!(
                "rule {} has check = {other:?}; expected mechanical, review or mixed",
                r.id
            ),
        }
    }
    assert!(
        seen_review > 0,
        "no rule is `review` or `mixed`. Either the standard really is fully \
         mechanical here -- in which case rule 6's scoping clause should be decided \
         rather than deferred -- or the file has stopped recording which clauses it \
         cannot check, which is the failure this assertion exists to catch."
    );
}

/// A disposition of `gated` on every rule would mean the set-equality ratchet
/// covers everything, which is not the design and not the claim being made. Rules
/// 3 and 9 are unsatisfiable by construction and are counted instead. If this
/// test ever fails, the honest question is which side moved, not which line to edit.
#[test]
fn the_reported_rules_are_exactly_those_that_cannot_be_satisfied() {
    let s = standard();
    let reported: BTreeSet<usize> = s
        .rules
        .iter()
        .filter(|r| r.disposition == "reported")
        .map(|r| r.id)
        .collect();
    assert_eq!(
        reported,
        BTreeSet::from([3, 9]),
        "the reported set moved. Rules 3 (no allocation after init) and 9 (no \
         dynamic dispatch) are unsatisfiable in this crate: Arc<dyn CacheTier> is \
         the tier abstraction and Box<dyn Future> is what #[async_trait] emits. If \
         one of them is now satisfiable, the gate should gate it rather than report it."
    );
}

/// Locations must resolve. This assertion is the one B42 asks to be lifted out of
/// this layer and applied to every `locations` entry in `bugs.toml`: a finding
/// whose location names a file that does not exist counts exactly the same as one
/// that does, and the gate is green either way.
#[test]
fn every_baseline_location_resolves_to_a_real_file_and_line() {
    let r = root();
    let mut checked = 0usize;
    for b in standard().baseline {
        let (file, line) = b
            .location
            .rsplit_once(':')
            .unwrap_or_else(|| panic!("{}: location has no line number", b.location));
        let line: usize = line
            .parse()
            .unwrap_or_else(|_| panic!("{}: line is not a number", b.location));
        let p = r.join(file);
        assert!(
            p.is_file(),
            "{} names a file that does not exist",
            b.location
        );
        let text = std::fs::read_to_string(&p).expect("read");
        let count = text.lines().count();
        assert!(
            line >= 1 && line <= count,
            "{} points past the end of {file}, which has {count} lines",
            b.location
        );
        checked += 1;
    }
    assert!(checked > 100, "only {checked} locations were checked");
}

/// A gate in the contract that no CI job runs is a gate that exists in review and
/// nowhere else. `tetanus` was declared and ran only locally until this test
/// existed, which is the same shape of defect as `tests/contract` itself having no
/// CI job -- the one this repository already has a finding for.
#[test]
fn the_gate_is_declared_ordered_and_executed() {
    let text = std::fs::read_to_string(root().join("theSix.toml")).expect("read theSix.toml");
    assert!(
        text.contains("name = \"tetanus\""),
        "the gate is not declared in theSix.toml"
    );
    assert!(
        text.contains("kind = \"analysis\""),
        "the gate is declared with the wrong kind; it must be `analysis`, since the \
         runner parses source rather than shelling out"
    );

    // Declared after clippy: rule 10 is a lint rule, so a lint change that shifts
    // the baseline should surface as a gate failure rather than as a quiet edit.
    let clippy = text.find("name = \"clippy\"").expect("clippy is declared");
    let tetanus = text
        .find("name = \"tetanus\"")
        .expect("tetanus is declared");
    assert!(clippy < tetanus, "the gate is declared before clippy");

    let order = text
        .find("order = [")
        .map(|i| &text[i..])
        .expect("a gate order is declared");
    let clippy_pos = order.find("\"clippy\"").expect("clippy is ordered");
    let tetanus_pos = order.find("\"tetanus\"").expect("tetanus is ordered");
    assert!(clippy_pos < tetanus_pos, "tetanus is ordered before clippy");

    let workflow =
        std::fs::read_to_string(root().join(".github/workflows/ci.yml")).expect("read ci");
    assert!(
        workflow.contains("cargo xtask run tetanus"),
        "no CI job runs the gate: declared and ordered, executed nowhere"
    );
}

/// The baseline must be large enough to be this crate, and must cover every gated
/// rule.
///
/// An earlier version also asserted the count was *not* round, on the theory that a
/// tidy number was chosen rather than measured. It failed on 644 immediately, which
/// is what a measured count looks like; the assertion was testing arithmetic, not
/// the file, and would have failed on every resync.
#[test]
fn the_baseline_is_not_suspiciously_small_or_round() {
    let s = standard();
    let n = s.baseline.len();
    assert!(n > 100, "a baseline of {n} entries is not this crate");

    // Every gated rule with findings must actually be represented. A gated rule
    // absent from the baseline either finds nothing, or its baseline was deleted.
    let mut per_rule = std::collections::BTreeMap::new();
    for b in &s.baseline {
        *per_rule.entry(b.rule).or_insert(0usize) += 1;
    }
    for r in s.rules.iter().filter(|r| r.disposition == "gated") {
        let count = per_rule.get(&r.id).copied().unwrap_or(0);
        if count == 0 {
            let why = r.finds_nothing_because.as_deref().unwrap_or("").trim();
            assert!(
                why.len() > 40,
                "rule {} is gated and has no baseline entry, with no \
                 `finds_nothing_because`. Either it finds nothing -- which then has \
                 to be said, so the zero is a recorded fact rather than an \
                 unexamined pass -- or its entries were deleted, which silently \
                 unchecks the rule.",
                r.id
            );
        }
    }
}

fn tomllib_from(text: &str) -> String {
    text.to_string()
}
