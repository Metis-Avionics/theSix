//! Every declared invariant is bound to a proving test, or explicitly waived.
//!
//! This is the check that closes the gap a second review named: the contract
//! asserted that its flags were set, its layers had targets and its negative
//! cases had cases, and never that a declared *runtime invariant* had any test
//! exercising it. One clause — the move recovery direction — was covered, by
//! hand. So a new invariant could be declared on the strength of a good sentence
//! and the gate would stay green, which is the over-promise this crate exists to
//! make impossible.
//!
//! The mechanism is deliberately split so neither side can satisfy itself:
//!
//! * `testkit::coverage::INVARIANT_PROOFS` — invariant to `path::fn`. Test-side
//!   knowledge: what the suite exercises.
//! * `theSix.toml` `[[verification.invariant_waiver]]` — invariant to reason.
//!   Contract-side knowledge: what is admitted to be unverified.
//!
//! The union of the two must equal the declared invariant set exactly. Adding an
//! invariant without a test or a waiver fails. Renaming one fails, because the
//! old key becomes an orphan. Deleting one fails, because its proof becomes an
//! orphan. Removing a waiver without adding a proof fails.
//!
//! What this does **not** do is check that the named test proves the invariant.
//! Naming a test that ignores the clause satisfies this gate. That is the same
//! limit `declare_cases!` documents, and the enforcement here is the part that
//! can be mechanical: an invariant cannot be declared into existence without
//! something in this repository being made responsible for it.

use std::collections::BTreeSet;

use testkit::coverage::{INVARIANT_PROOFS, missing_locators, repo_root};

const CONTRACT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/theSix.toml"));

/// The seven sections whose leaves are runtime invariants.
///
/// `spec`, `verification`, `testing`, `observability` and `engineering` are
/// excluded: they describe the *checking apparatus*, not the cache. Including
/// them would make the gate assert that its own configuration is an invariant,
/// which is circular.
const SEMANTIC_SECTIONS: &[&str] = &[
    "acid",
    "cia",
    "hpa",
    "continuity",
    "authority",
    "capabilities",
    "concurrency",
];

/// Every invariant leaf the contract declares, as dotted paths.
///
/// A leaf counts if it is a bool or a string. Bools are the negative-form
/// guarantees (`partial_commit_visible = false`). Strings matter because several
/// clauses are normative values rather than flags — `commit_protocol =
/// "two-phase-intent"`, `read_of_uncommitted_entry = "miss"`,
/// `recovery_direction_prepare_move = "external-reconciliation"`. A checker that
/// only understood bools would quietly exempt exactly the clauses that are
/// hardest to read as requirements.
fn declared_invariants() -> BTreeSet<String> {
    let doc: toml::Value = toml::from_str(CONTRACT).expect("theSix.toml must parse");
    let table = doc.as_table().expect("the contract is a table");

    let mut found = BTreeSet::new();
    for section in SEMANTIC_SECTIONS {
        let node = table
            .get(*section)
            .unwrap_or_else(|| panic!("theSix.toml must declare [{section}]"));
        collect_leaves(node, section, &mut found);
    }
    found
}

fn collect_leaves(node: &toml::Value, prefix: &str, out: &mut BTreeSet<String>) {
    let Some(table) = node.as_table() else { return };
    for (key, value) in table {
        let path = format!("{prefix}.{key}");
        match value {
            toml::Value::Boolean(_) | toml::Value::String(_) => {
                out.insert(path);
            }
            toml::Value::Table(_) => collect_leaves(value, &path, out),
            // Arrays and floats are configuration (layer lists, thresholds), not
            // guarantees about the cache's behaviour.
            _ => {}
        }
    }
}

fn waiver_invariants() -> BTreeSet<String> {
    #[derive(serde::Deserialize)]
    struct Root {
        verification: Verification,
    }
    #[derive(serde::Deserialize)]
    struct Verification {
        #[serde(default)]
        invariant_waiver: Vec<Waiver>,
    }
    #[derive(serde::Deserialize)]
    struct Waiver {
        invariant: String,
        #[serde(default)]
        reason: String,
    }

    let root: Root = toml::from_str(CONTRACT).expect("theSix.toml must parse");
    for w in &root.verification.invariant_waiver {
        assert!(
            !w.reason.trim().is_empty(),
            "waiver for `{}` has no reason. A waiver without a reason is an \
             unexplained hole; the sentence is the part a later reader inherits.",
            w.invariant
        );
    }
    root.verification
        .invariant_waiver
        .into_iter()
        .map(|w| w.invariant)
        .collect()
}

#[test]
fn every_declared_invariant_has_a_proof_or_a_waiver() {
    let declared = declared_invariants();
    let proven: BTreeSet<&str> = INVARIANT_PROOFS.iter().map(|(k, _)| *k).collect();
    let waived = waiver_invariants();

    let uncovered: Vec<&String> = declared
        .iter()
        .filter(|k| !proven.contains(k.as_str()) && !waived.contains(*k))
        .collect();
    assert!(
        uncovered.is_empty(),
        "{} declared invariants have neither a proving test nor a waiver: {uncovered:#?}.\n\
         Declaring an invariant must cost something. Add it to \
         `testkit::coverage::INVARIANT_PROOFS` with the test that exercises it, or \
         to `[[verification.invariant_waiver]]` with a sentence saying why it is \
         unverified. A clause nobody is responsible for is how this contract \
         over-promised in the first place.",
        uncovered.len()
    );

    assert!(
        declared.len() >= 90,
        "only {} invariants were discovered in the semantic sections; the walker is \
         probably broken rather than the contract small",
        declared.len()
    );
}

#[test]
fn no_invariant_is_both_proven_and_waived() {
    let proven: BTreeSet<&str> = INVARIANT_PROOFS.iter().map(|(k, _)| *k).collect();
    let waived = waiver_invariants();

    let both: Vec<&String> = waived
        .iter()
        .filter(|k| proven.contains(k.as_str()))
        .collect();
    assert!(
        both.is_empty(),
        "these invariants are recorded as both proven and waived: {both:?}. One of the \
         two records is stale."
    );
}

#[test]
fn no_proof_or_waiver_names_an_invariant_that_does_not_exist() {
    let declared = declared_invariants();
    let waived = waiver_invariants();

    let stale_proofs: Vec<&str> = INVARIANT_PROOFS
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| !declared.contains(*k))
        .collect();
    assert!(
        stale_proofs.is_empty(),
        "INVARIANT_PROOFS names invariants the contract no longer declares: \
         {stale_proofs:?}. Renaming or deleting an invariant in theSix.toml has to \
         change this registry too, or the proof outlives the claim."
    );

    let stale_waivers: Vec<&String> = waived.iter().filter(|k| !declared.contains(*k)).collect();
    assert!(
        stale_waivers.is_empty(),
        "[[verification.invariant_waiver]] names invariants the contract no longer \
         declares: {stale_waivers:?}."
    );
}

#[test]
fn every_named_proving_test_exists() {
    // The walk itself lives in `testkit::coverage::missing_locators` because two
    // other registries now cite tests the same way. A private copy here would
    // have been the third spelling of one rule.
    let missing = missing_locators(&repo_root(), INVARIANT_PROOFS);

    assert!(
        missing.is_empty(),
        "INVARIANT_PROOFS cites tests that do not exist.\n  {}\n\
         A registry that names a phantom test is worse than no registry: it reads \
         as coverage while proving nothing.",
        missing.join("\n  ")
    );
}

#[test]
fn a_proof_may_not_point_at_this_module() {
    // Not a rule, a smell. `tests/contract/invariant.rs` is the file that checks
    // the registry, so a proof pointing here means the invariant is "verified" by
    // the bookkeeping that records the verification.
    let self_referential: Vec<&str> = INVARIANT_PROOFS
        .iter()
        .filter(|(_, locator)| locator.contains("tests/contract/invariant.rs::"))
        .map(|(k, _)| *k)
        .filter(|k| !k.ends_with(".required"))
        .collect();

    assert!(
        self_referential.is_empty(),
        "these invariants cite tests/contract/invariant.rs, which is the module that \
         audits the registry: {self_referential:?}. A real invariant needs a test \
         that exercises the runtime. (Invariants named `*.required` are exempt: \
         they are contract meta-flags, and asserting they are set is genuinely all \
         they mean.)"
    );
}

/// The thirteen `required = true` flags under the semantic sections.
///
/// These were previously asserted by nothing at all: `required_verification_flags_
/// all_set` checks `verification.required`, a different table. So a section could
/// be quietly marked as not-required and the gate would not notice — which inverts
/// the meaning of the flag.
#[test]
fn every_semantic_section_is_marked_required() {
    testkit::proves!(
        "acid.atomicity.required",
        "acid.consistency.required",
        "acid.durability.required",
        "acid.isolation.required",
        "acid.required",
        "capabilities.required",
        "cia.availability.required",
        "cia.confidentiality.required",
        "cia.integrity.required",
        "cia.required",
        "concurrency.required",
        "continuity.required",
        "hpa.required"
    );

    let doc: toml::Value = toml::from_str(CONTRACT).expect("theSix.toml must parse");
    let table = doc.as_table().expect("the contract is a table");

    let mut cleared = Vec::new();
    let mut checked = 0usize;

    for section in SEMANTIC_SECTIONS {
        let node = table
            .get(*section)
            .unwrap_or_else(|| panic!("theSix.toml must declare [{section}]"));
        check_required(node, section, &mut cleared, &mut checked);
    }

    assert!(
        cleared.is_empty(),
        "these `required` flags are not true: {cleared:?}. Every section and \
         subsection under the semantic contract declares its guarantees required; \
         clearing one withdraws the guarantee without removing the claim."
    );
    // A floor, not an exact count: it catches the walk silently finding nothing,
    // which is how this check would otherwise pass having verified no flags.
    assert!(
        checked >= 13,
        "only {checked} `required` flags were found; the walk is probably broken"
    );
}

fn check_required(
    node: &toml::Value,
    prefix: &str,
    cleared: &mut Vec<String>,
    checked: &mut usize,
) {
    let Some(table) = node.as_table() else { return };
    for (key, value) in table {
        let path = format!("{prefix}.{key}");
        match value {
            toml::Value::Boolean(flag) if key == "required" => {
                *checked += 1;
                if !flag {
                    cleared.push(path);
                }
            }
            toml::Value::Table(_) => check_required(value, &path, cleared, checked),
            _ => {}
        }
    }
}
