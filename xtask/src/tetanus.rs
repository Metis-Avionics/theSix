//! The TETANUS gate: NASA/JPL's *Power of Ten*, adapted to Rust, enforced as a
//! ratchet against a declared baseline.
//!
//! The rules are Gerard J. Holzmann's (NASA/JPL, IEEE Computer, June 2006). The
//! Rust mapping is this repository's own work and is not endorsed by JPL or by
//! Holzmann; `tetanus.toml` says so in `[meta]` so no reader has to infer it.
//!
//! # Why a bespoke parser rather than a lint
//!
//! The question this gate answers is not "is this code well written" but "is the
//! set of violations still exactly the set somebody looked at and justified".
//! No linter answers that, because it has no memory of the last run. So the gate
//! parses the tree itself and compares two sets for equality, in both directions.
//!
//! That equality is the load-bearing property. A finding with no entry fails, so
//! new violations cannot land. An entry with no finding fails, so a justification
//! cannot outlive the code it justified and quietly stop being checked. The second
//! direction is the one that makes the first trustworthy: an analyzer that gets
//! *weaker* produces fewer findings, which shows up as stale entries rather than as
//! an improvement.
//!
//! # Disposition: gated versus reported
//!
//! Rules 3 and 9 are recognised and reported but do not participate in the
//! ratchet. Both are unsatisfiable by construction in this crate -- `Arc<dyn
//! CacheTier<V>>` *is* the tier abstraction, and `Box<dyn Future>` is what
//! `#[async_trait]` emits -- so baselining them would mean committing ~1000
//! justified entries that say the same thing, and the count would be the only
//! thing anyone ever looked at. They are counted and printed instead, which keeps
//! the number visible without pretending the number is an achievement.
//!
//! Whether they should instead be *satisfied*, at the cost of a breaking API
//! change, is an open question and not this gate's to settle. See
//! `.kilo/plans/1789334873419-tetanus-rule3-plan.md` for the argument that it
//! should be.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::gates::{GateResult, Outcome};

// ---------------------------------------------------------------------------
// Contract: tetanus.toml
// ---------------------------------------------------------------------------

/// How a rule is decided. The distinction exists because a rule with a clause the
/// gate cannot decide must say so, rather than being rounded to the nearest
/// category the gate does understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// Every site is decided by the gate.
    Mechanical,
    /// No site is decided by the gate; a human decides, and the rule names the
    /// artefact where that judgement is recorded.
    Review,
    /// One clause is mechanical and one is not. Both rules 5 and 7 are in this
    /// state, and rounding either to `Mechanical` or to `Review` is a false
    /// statement about what is checked.
    Mixed,
}

/// Whether a rule participates in the set-equality ratchet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Counted against the baseline. A finding without an entry fails, and an
    /// entry without a finding fails.
    Gated,
    /// Counted and printed, never baselined. Used for rules that cannot be
    /// satisfied without breaking the public API, where the honest report is a
    /// number and a reason rather than a wall of identical justifications.
    Reported,
}

#[derive(Debug, Deserialize)]
struct File {
    meta: Meta,
    /// `[[rule]]` in TOML, not `[[rules]]`. The rename is required: serde matches
    /// the field name by default, so an unannotated `rules` silently deserialises
    /// as empty and every rule reads as undeclared.
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

#[derive(Debug, Clone, Deserialize)]
struct Rule {
    id: usize,
    title: String,
    check: Check,
    disposition: Disposition,
    /// Where a human's judgement about the undecidable clause is recorded.
    /// Required for `Review` and `Mixed`, forbidden for `Mechanical`: a
    /// mechanical rule that names an artefact is claiming a review it does not do.
    #[serde(default)]
    review_artifact: Option<String>,
}

/// One declared existing violation.
///
/// The site is `(rule, location)`. `reason` is mandatory and may not be a
/// placeholder, because the gate cannot tell a real justification from a
/// well-formed one -- only a person can, and a `TODO` is a promise to return.
#[derive(Debug, Clone, Deserialize)]
struct Baseline {
    rule: usize,
    location: String,
    reason: String,
}

/// A site the checker found.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub rule: usize,
    pub location: String,
    pub detail: String,
}

const PLACEHOLDER_MARKERS: [&str; 4] = ["TODO", "FIXME", "XXX", "placeholder"];

// ---------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Verdict {
    findings: Vec<Finding>,
    parse_failures: Vec<String>,
    scanned: usize,
}

impl Verdict {
    fn is_clean(&self) -> bool {
        self.parse_failures.is_empty()
    }
}

// ---------------------------------------------------------------------------
// File discovery
// ---------------------------------------------------------------------------

fn discover(roots: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for root in roots {
        let dir = Path::new(root);
        if !dir.is_dir() {
            return Err(format!("scan root `{root}` does not exist"));
        }
        walk(dir, &mut out)?;
    }
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ---------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------

/// Findings for one parsed file. `rel` is the repo-relative path, used to build
/// every location so a finding is addressable from the repository root rather
/// than from wherever the checker happened to be invoked.
fn scan(rel: &str, file: &syn::File) -> Vec<Finding> {
    let mut found = Vec::new();
    let mut v = Scan {
        rel,
        out: &mut found,
    };
    for item in &file.items {
        v.item(item);
    }
    found
}

struct Scan<'a> {
    rel: &'a str,
    out: &'a mut Vec<Finding>,
}

impl Scan<'_> {
    fn push(&mut self, rule: usize, line: usize, detail: impl Into<String>) {
        self.out.push(Finding {
            rule,
            location: format!("{}:{}", self.rel, line),
            detail: detail.into(),
        });
    }

    fn item(&mut self, item: &syn::Item) {
        // Rule 8: conditional compilation and macro-generated code. Checked at the
        // item level so a `#[cfg]`-gated block is attributed to the item it gates
        // rather than to whatever line the attribute happens to sit on.
        if let Some(attr) = item_attrs(item) {
            if let Some(line) = line_of(&attr.path()) {
                let detail = describe_attr(&attr.path());
                self.push(8, line, detail);
            }
        }

        // Rules 1, 2 and 4 all need a named function and its statements, so they
        // are decided once here rather than by three visitors over the same tree.
        if let Some(fn_) = as_fn(item) {
            self.function(&fn_.sig.ident.to_string(), fn_);
        }

        syn::visit::visit_item(self, item);
    }

    fn function(&mut self, name: &str, item: &syn::ItemFn) {
        let line = line_of(&item.sig.ident).unwrap_or(0);

        if item.sig.unsafety.is_some() {
            self.push(9, line, format!("`{name}` is `unsafe fn`"));
        }

        // Rule 1. Deliberately narrow: a call through a `syn::Expr::Path` that
        // names this function, or a method call whose receiver is literally
        // `self`. Both are decidable without a type resolver. A cycle that runs
        // through `self.other.field()` is *not* found, and `tetanus.toml` says so
        // rather than leaving the silence to be discovered later.
        if recurses(name, &item.block) {
            self.push(
                1,
                line,
                format!("`{name}` calls itself with no intervening state change"),
            );
        }

        // Rule 2. A loop whose every exit is conditional may not terminate. The
        // test is deliberately strict about *what counts as an exit*: an
        // unconditional `break`/`return`, or a `panic!`. A `break` nested inside
        // an `if` is not an exit, and treating it as one is what produced eight
        // findings here before it was tightened to nineteen.
        if let Some((kind, _)) = unbounded_loop(&item.block) {
            self.push(
                2,
                line,
                format!(
                    "`{name}` has an unbounded `{kind}`: no exit is reached on every \
                         iteration"
                ),
            );
        }

        // Rule 4, counted in *statements*, not lines. `set()` is 230 physical lines
        // and about 98 code lines; the difference is comments documenting two
        // failure windows, and a line count would demand a refactor the code does
        // not need. This also agrees with `clippy::too_many_lines`, which is live
        // in this crate and skips comments.
        let statements = statement_count(&item.block);
        if statements > 60 {
            self.push(
                4,
                line,
                format!(
                    "`{name}` holds {statements} statements; the limit is 60 (lines, \
                     comments excluded, which is also how clippy counts)"
                ),
            );
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    /// Rule 5: a `match` whose arms mix error classes with data classes. A
    /// function that returns `Result` in one arm and a bare value in another has
    /// no single answer to "did this succeed", which is the failure this rule is
    /// about. Reported per match site.
    fn visit_expr_match(&mut self, node: &syn::ExprMatch) {
        if let Some(line) = line_of(&node.expr) {
            let data = node.arms.iter().filter(|a| arm_returns_value(a)).count();
            let error = node.arms.iter().filter(|a| arm_returns_result(a)).count();
            if data > 0 && error > 0 {
                self.push(
                    5,
                    line,
                    format!("match mixes {data} value arm(s) with {error} result arm(s)"),
                );
            }
        }
        syn::visit::visit_expr_match(self, node);
    }

    /// Rule 7: check-then-act.
    ///
    /// One branch diverges and the other yields a value, so whether a value exists
    /// at all depends on control flow that a caller cannot see. The published rule
    /// is about a *check* separated from the *act*; this is its local form, and
    /// `tetanus.toml` records that the two race windows this clause really exists
    /// for are named as findings B19 and B22 rather than found here.
    ///
    /// Both branches must be present, and an `else if` chain counts as one branch
    /// group rather than a branch. A conditional with no `else` is deliberately
    /// *not* reported even though its fall-through arguably supplies the missing
    /// branch: every early `return` in the crate would be a finding, which is a
    /// false-positive generator, and a `mixed` rule should err toward missing a
    /// site rather than inventing one. The two races this clause exists for are
    /// named as findings B19 and B22, with reproductions.
    fn visit_expr_if(&mut self, node: &syn::ExprIf) {
        if let Some(line) = line_of(&node.cond) {
            let then_diverges = block_diverges(&node.then_branch);
            let then_yields = block_yields_value(&node.then_branch);
            let (else_diverges, else_yields) = match &node.else_branch {
                Some((_, e)) => match &**e {
                    syn::Expr::Block(b) => (block_diverges(&b.block), block_yields_value(&b.block)),
                    // `else if ...` is a continuation of the same conditional, not a
                    // second branch, so it neither diverges nor yields here.
                    syn::Expr::If(_) => (false, false),
                    _ => (false, true),
                },
                None => (false, false),
            };
            if (then_diverges && else_yields) || (else_diverges && then_yields) {
                self.push(
                    7,
                    line,
                    "one branch of this conditional diverges and the other yields a value, \
                     so whether a value exists depends on control flow the caller \
                     cannot see"
                        .to_string(),
                );
            }
        }
        syn::visit::visit_expr_if(self, node);
    }

    /// Rule 3: a heap allocation, or a call that almost certainly makes one.
    ///
    /// Declared `reported` rather than `gated`, so it is not baselined -- but it is
    /// still *checked*, and that distinction matters. An earlier version of this
    /// file declared the rule with no check behind it, which made it report zero
    /// sites and read as "this crate never allocates after init". It allocates
    /// several hundred times. A rule that finds nothing because nothing looks is
    /// worse than no rule: it converts a known gap into an apparent pass.
    ///
    /// The detection is deliberately syntactic and therefore over-approximate. For
    /// a rule whose disposition is `reported`, a count that is too high costs
    /// nothing, and a count that is too low would be the dangerous direction.
    fn visit_expr(&mut self, node: &syn::Expr) {
        if let Some(detail) = allocation_detail(node)
            && let Some(line) = line_of(node)
        {
            self.push(3, line, detail);
        }
        syn::visit::visit_expr(self, node);
    }

    /// Rule 6: static or global state.
    ///
    /// A `static` in a cache crate is a shared mutable cell wearing a name that
    /// says "constant", so it is checked rather than assumed absent. Rule 6 is
    /// `review` because the interesting question is not whether a `static` exists
    /// but whether its scope is the crate's or a test's -- a tree can answer the
    /// first and not the second, which is the clause `tetanus.toml` defers.
    fn visit_item_static(&mut self, node: &syn::ItemStatic) {
        if let Some(line) = line_of(&node.ident) {
            // `StaticMutability` is non-exhaustive, so a wildcard rather than an
            // exhaustive match: a future variant must not become a compile error
            // here, because a gate that fails to compile is a gate nobody runs.
            let mutability = if matches!(node.mutability, syn::StaticMutability::Mut(_)) {
                "mutable "
            } else {
                "immutable "
            };
            self.push(
                6,
                line,
                format!(
                    "`static {mutability}` `{}`: scope is this crate's for the whole \
                     process, so a test that touches it is testing shared state",
                    node.ident
                ),
            );
        }
        syn::visit::visit_item_static(self, node);
    }

    /// Rule 9: a bare trait object in a type position, or a boxed trait object.
    /// Both are the "no dynamic dispatch" clause. Recognised and reported, never
    /// gated -- see the module docs.
    fn visit_type(&mut self, node: &syn::Type) {
        let detail = match node {
            syn::Type::TraitObject(_) => Some(format!("bare trait object `{}`", print_type(node))),
            syn::Type::Path(p) => p
                .path
                .segments
                .last()
                .filter(|s| s.ident == "Box")
                .and_then(|_| dyn_under(node))
                .map(|_| "`Box<dyn ..>`: indirection on indirection".to_string()),
            _ => None,
        };
        if let Some(detail) = detail
            && let Some(line) = line_of(node)
        {
            self.push(9, line, detail);
        }
        syn::visit::visit_type(self, node);
    }

    /// Rule 10: every `#[allow]`/`#[expect]`. The crate is `#![deny(warnings)]` and
    /// clippy runs pedantic, so a suppression is a claim that is checked here but
    /// not justified there. Gated per site, which is the point: an unexplained
    /// `allow` is the thing worth noticing.
    fn visit_attribute(&mut self, node: &syn::Attribute) {
        let name = node.path().segments.last().map(|s| s.ident.to_string());
        if let Some(kw @ ("allow" | "expect")) = name.as_deref() {
            if let Some(line) = line_of(node) {
                let lint = match &node.meta {
                    syn::Meta::List(l) => quote::ToTokens::to_token_stream(&l.tokens).to_string(),
                    other => quote::ToTokens::to_token_stream(other).to_string(),
                }
                .replace('"', "")
                .trim_start_matches("allow")
                .trim_start_matches("expect")
                .trim()
                .to_string();
                self.push(
                    10,
                    line,
                    format!(
                        "`#[{kw}({lint})]` suppresses a diagnostic the gate would \
                         otherwise treat as a finding"
                    ),
                );
            }
        }
        syn::visit::visit_attribute(self, node);
    }
}

// ---------------------------------------------------------------------------
// Rule predicates
// ---------------------------------------------------------------------------

fn item_attrs(item: &syn::Item) -> Option<&syn::Attribute> {
    let attrs: &[syn::Attribute] = match item {
        syn::Item::Fn(f) => &f.attrs,
        syn::Item::Mod(m) => &m.attrs,
        syn::Item::Struct(s) => &s.attrs,
        syn::Item::Enum(e) => &e.attrs,
        syn::Item::Trait(t) => &t.attrs,
        syn::Item::Impl(i) => &i.attrs,
        _ => return None,
    };
    attrs.first()
}

fn describe_attr(path: &syn::Path) -> String {
    let name = path
        .segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default();
    match name.as_str() {
        "cfg" => "conditional compilation: this code is not always compiled, so \
                  neither is its verification"
            .to_string(),
        "cfg_attr" => "conditional attribute: this code is not always compiled, so \
                       neither is its verification"
            .to_string(),
        _ => {
            format!("`#[{name}]`: this code is not always compiled, so neither is its verification")
        }
    }
}

/// Rule 1. A named call to this function, or a method call on `self`.
///
/// `Expr::Path` only. A bare `foo(n - 1)` is an `Expr::Path`; a bare `foo` used
/// as a value is an `Expr::Path` too, so the callee is required to be a function
/// call. Treating the value case as recursion produced a false positive on every
/// closure that merely mentions the function, which is why the callee check is
/// here and not in the visitor.
fn recurses(name: &str, block: &syn::Block) -> bool {
    struct Rec {
        name: String,
        found: bool,
    }
    impl<'ast> Visit<'ast> for Rec {
        fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
            if self.found {
                return;
            }
            if let syn::Expr::Path(p) = &*node.func
                && p.path.is_ident(self.name.as_str())
            {
                self.found = true;
                return;
            }
            if let syn::Expr::MethodCall(m) = &*node.func
                && matches!(&*m.receiver, syn::Expr::Path(p) if p.path.is_ident("self"))
            {
                self.found = true;
                return;
            }
            syn::visit::visit_expr_call(self, node);
        }
    }
    let mut r = Rec {
        name: name.to_string(),
        found: false,
    };
    r.visit_block(block);
    r.found
}

/// Rule 2. Returns the first loop in the body that may not terminate, and
/// whether it has an unconditional exit.
///
/// An exit is a `break` that is reached on every iteration: not nested inside an
/// `if`, a `match`, or a short-circuiting `&&`/`||`, and not inside a nested loop
/// whose own `break` only leaves that loop. A `return` or a `panic!` is an exit
/// whatever it is nested in, because either one leaves the loop.
///
/// The conditional case is not a detail. Counting a `break` under an `if` as an
/// exit reported these loops as bounded when they are not, which is the false
/// negative a gate in this position cannot afford.
fn unbounded_loop(block: &syn::Block) -> Option<(&'static str, bool)> {
    struct Loops {
        out: Option<(&'static str, bool)>,
    }
    impl Visit<'_> for Loops {
        fn visit_expr_loop(&mut self, node: &syn::ExprLoop) {
            if self.out.is_none() && !has_exit(&node.body) {
                self.out = Some(("loop", false));
            }
            // Recurse to find loops in arms, blocks and nested items either way.
            syn::visit::visit_expr_loop(self, node);
        }
        fn visit_expr_while(&mut self, node: &syn::ExprWhile) {
            if self.out.is_none() && !has_exit(&node.body) {
                self.out = Some(("while", false));
            }
            syn::visit::visit_expr_while(self, node);
        }
        fn visit_expr_for_loop(&mut self, node: &syn::ExprForLoop) {
            if self.out.is_none() && !has_exit(&node.body) {
                self.out = Some(("for", false));
            }
            syn::visit::visit_expr_for_loop(self, node);
        }
    }
    let mut l = Loops { out: None };
    l.visit_block(block);
    l.out
}

/// Whether `body` has an exit that is reached on every iteration of the loop that
/// owns it.
fn has_exit(body: &syn::Block) -> bool {
    struct Ex {
        /// How many conditionals we are inside. A `break` below this does not
        /// terminate the loop on every iteration.
        cond: usize,
        /// How many nested loops we are inside. A `break` below this leaves that
        /// loop, not ours.
        nested_loops: usize,
        found: bool,
    }
    impl Visit<'_> for Ex {
        fn visit_expr_break(&mut self, node: &syn::ExprBreak) {
            // A labelled `break` targets an outer loop by name, so it is not this
            // loop's exit.
            if node.label.is_none() && self.cond == 0 && self.nested_loops == 0 {
                self.found = true;
            }
            syn::visit::visit_expr_break(self, node);
        }
        fn visit_expr_return(&mut self, _: &syn::ExprReturn) {
            // Leaves the function, so it leaves every loop in it.
            self.found = true;
        }
        fn visit_expr_macro(&mut self, node: &syn::ExprMacro) {
            if node
                .mac
                .path
                .segments
                .last()
                .is_some_and(|s| s.ident == "panic")
            {
                self.found = true;
            }
            syn::visit::visit_expr_macro(self, node);
        }
        fn visit_expr_if(&mut self, node: &syn::ExprIf) {
            self.cond += 1;
            syn::visit::visit_expr_if(self, node);
            self.cond -= 1;
        }
        fn visit_expr_match(&mut self, node: &syn::ExprMatch) {
            self.cond += 1;
            syn::visit::visit_expr_match(self, node);
            self.cond -= 1;
        }
        fn visit_expr_closure(&mut self, node: &syn::ExprClosure) {
            // A closure body is not the loop's body. Its `break` would not even
            // compile, but its `return` returns from the loop's function, which
            // `found` already covers by visiting it as a value.
            self.cond += 1;
            syn::visit::visit_expr_closure(self, node);
            self.cond -= 1;
        }
        fn visit_expr_loop(&mut self, node: &syn::ExprLoop) {
            if self.found {
                return;
            }
            self.nested_loops += 1;
            syn::visit::visit_expr_loop(self, node);
            self.nested_loops -= 1;
        }
        fn visit_expr_while(&mut self, node: &syn::ExprWhile) {
            if self.found {
                return;
            }
            self.nested_loops += 1;
            syn::visit::visit_expr_while(self, node);
            self.nested_loops -= 1;
        }
        fn visit_expr_for_loop(&mut self, node: &syn::ExprForLoop) {
            if self.found {
                return;
            }
            self.nested_loops += 1;
            syn::visit::visit_expr_for_loop(self, node);
            self.nested_loops -= 1;
        }
        fn visit_item_fn(&mut self, _: &syn::ItemFn) {
            // A nested item's `break`/`return` are its own.
        }
    }
    let mut e = Ex {
        cond: 0,
        nested_loops: 0,
        found: false,
    };
    e.visit_block(body);
    e.found
}

/// Rule 4. Statement count of a function body, nested closures included (their
/// statements do execute when the function runs) and nested functions excluded
/// (they are counted against themselves).
fn statement_count(block: &syn::Block) -> usize {
    struct Count(usize);
    impl Visit<'_> for Count {
        fn visit_stmt(&mut self, node: &syn::Stmt) {
            self.0 += 1;
            match node {
                syn::Stmt::Item(syn::Item::Fn(_)) | syn::Stmt::Item(syn::Item::Macro(_)) => {
                    syn::visit::visit_stmt(self, node);
                }
                _ => syn::visit::visit_stmt(self, node),
            }
        }
        fn visit_item_fn(&mut self, _: &syn::ItemFn) {}
    }
    let mut c = Count(0);
    c.visit_block(block);
    c.0
}

/// An arm that yields a value: a non-empty block, or a bare expression.
///
/// Every arm that is not a diverging one yields a value, so this is the complement
/// of "diverges" rather than a list of shapes. Written as a complement on purpose:
/// enumerating shapes missed `Ok(x) => x` and reported rule 5 as clean.
fn arm_returns_value(arm: &syn::Arm) -> bool {
    match &*arm.body {
        syn::Expr::Block(b) => !b.block.stmts.is_empty(),
        syn::Expr::Return(_) | syn::Expr::Break(_) | syn::Expr::Continue(_) => false,
        _ => true,
    }
}

fn arm_returns_result(arm: &syn::Arm) -> bool {
    match &*arm.body {
        syn::Expr::Block(b) => last_is_result(&b.block),
        syn::Expr::Path(p) => is_result_ident(p),
        syn::Expr::Call(c) => is_result_expr(&syn::Expr::Call(c.clone())),
        _ => false,
    }
}

fn last_is_result(block: &syn::Block) -> bool {
    matches!(block.stmts.last(), Some(syn::Stmt::Expr(e, _)) if is_result_expr(e))
        && block
            .stmts
            .last()
            .and_then(|s| match s {
                syn::Stmt::Expr(e, _) => Some(is_result_expr(e)),
                _ => None,
            })
            .unwrap_or(false)
}

fn is_result_expr(e: &syn::Expr) -> bool {
    match e {
        syn::Expr::Call(c) => match &*c.func {
            syn::Expr::Path(p) => is_result_ident(p),
            _ => false,
        },
        syn::Expr::Try(_) => true,
        syn::Expr::Block(b) => last_is_result(&b.block),
        _ => false,
    }
}

fn is_result_ident(p: &syn::ExprPath) -> bool {
    p.path
        .segments
        .last()
        .is_some_and(|s| s.ident == "Err" || s.ident == "Ok")
}

/// A block yields a value when its last statement is a trailing expression with no
/// semicolon, and that expression is not an exit.
///
/// The semicolon is the whole point. syn records `return 1;` as
/// `Stmt::Expr(Expr::Return(..), Some(semi))` and `2` as `Stmt::Expr(.., None)`, so
/// a pattern that ignored the semicolon read a `return` as a value and a `return`
/// as an absence -- inverting rule 7 at every site, which is why it found nothing.
fn block_yields_value(block: &syn::Block) -> bool {
    match block.stmts.last() {
        Some(syn::Stmt::Expr(e, None)) => !matches!(
            *e,
            syn::Expr::Return(_) | syn::Expr::Break(_) | syn::Expr::Continue(_)
        ),
        _ => false,
    }
}

/// A block diverges when its last statement is a `return` or `break`, with or
/// without a trailing semicolon -- see `block_yields_value` for why that
/// distinction matters.
fn block_diverges(block: &syn::Block) -> bool {
    match block.stmts.last() {
        Some(syn::Stmt::Expr(e, _)) => {
            matches!(*e, syn::Expr::Return(_) | syn::Expr::Break(_))
        }
        Some(syn::Stmt::Macro(m))
            if m.mac
                .path
                .segments
                .last()
                .is_some_and(|s| s.ident == "panic") =>
        {
            true
        }
        _ => false,
    }
}

/// The allocation a node represents, if any. Syntactic and over-approximate on
/// purpose; see `visit_expr`.
fn allocation_detail(node: &syn::Expr) -> Option<String> {
    // `format!`, `vec!`, `panic!` and friends.
    if let syn::Expr::Macro(m) = node {
        let name = m.mac.path.segments.last().map(|s| s.ident.to_string())?;
        if matches!(
            name.as_str(),
            "format" | "vec" | "write" | "writeln" | "format_args"
        ) {
            return Some(format!("`{name}!` allocates"));
        }
        return None;
    }
    // `Box::new(..)`, `Arc::new(..)`, `Vec::new()`, `String::new()`, `Box::pin(..)`
    // and friends: a path call whose final segment names an allocator and whose
    // first argument is not another such call.
    if let syn::Expr::Call(c) = node
        && let syn::Expr::Path(p) = &*c.func
        && let Some(seg) = p.path.segments.last()
    {
        let name = seg.ident.to_string();
        let is_allocator = matches!(
            name.as_str(),
            "new" | "pin" | "pin_mut" | "uninit" | "try_init" | "from" | "with_capacity"
        );
        if is_allocator
            && let Some(parent) = p
                .path
                .segments
                .iter()
                .rev()
                .nth(1)
                .map(|s| s.ident.to_string())
            && matches!(
                parent.as_str(),
                "Box" | "Rc" | "Arc" | "Vec" | "String" | "HashMap" | "BTreeMap"
            )
        {
            return Some(format!("`{parent}::{name}` allocates"));
        }
    }
    // Method-call forms, and the coercions that allocate without a macro.
    if let syn::Expr::MethodCall(m) = node {
        let name = m.method.to_string();
        if matches!(
            name.as_str(),
            "to_string"
                | "to_owned"
                | "to_vec"
                | "clone"
                | "collect"
                | "split_off"
                | "join"
                | "repeat"
        ) {
            return Some(format!("`.{name}()` allocates"));
        }
    }
    None
}

fn dyn_under(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => {
            let seg = p.path.segments.last()?;
            let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
                return None;
            };
            args.args.iter().find_map(|a| match a {
                syn::GenericArgument::Type(t) => match &*t {
                    syn::Type::TraitObject(o) => {
                        Some(print_type(&syn::Type::TraitObject(o.clone())))
                    }
                    _ => None,
                },
                _ => None,
            })
        }
        _ => None,
    }
}

fn print_type(ty: &syn::Type) -> String {
    use quote::ToTokens;
    ty.to_token_stream().to_string()
}

fn as_fn(item: &syn::Item) -> Option<&syn::ItemFn> {
    match item {
        syn::Item::Fn(f) => Some(f),
        _ => None,
    }
}

fn line_of<T: Spanned>(node: &T) -> Option<usize> {
    Some(node.span().start().line)
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

fn load(root: &Path) -> Result<File, String> {
    let path = root.join("tetanus.toml");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Runs the checker, producing findings for the whole tree.
fn collect(root: &Path, file: &File) -> Result<Verdict, String> {
    let mut verdict = Verdict::default();
    for path in discover(&file.meta.scan_roots)? {
        let rel = relative(root, &path);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                verdict.parse_failures.push(format!("{rel}: {e}"));
                continue;
            }
        };
        // A parse failure is a failure, not an absence. It is reported as such and
        // is never allowed to read as "this file is clean", because a synoptic
        // change that stops the parser would otherwise silently shrink the
        // baseline and turn the ratchet into a no-op.
        match syn::parse_file(&text) {
            Ok(parsed) => {
                verdict.scanned += 1;
                verdict.findings.extend(scan(&rel, &parsed));
            }
            Err(e) => verdict.parse_failures.push(format!("{rel}: {e}")),
        }
    }
    Ok(verdict)
}

/// What the gate decided, and why.
struct Report {
    lines: Vec<String>,
    clean: bool,
    unavailable: Option<String>,
}

fn analyse(root: &Path) -> Report {
    let file = match load(root) {
        Ok(f) => f,
        Err(e) => {
            return Report {
                lines: vec![format!("could not read the TETANUS contract: {e}")],
                clean: false,
                unavailable: Some(e),
            };
        }
    };

    let declared: BTreeMap<usize, &Rule> = {
        let mut m: BTreeMap<usize, &Rule> = BTreeMap::new();
        for r in &file.rules {
            if m.insert(r.id, r).is_some() {
                return Report {
                    lines: vec![format!("rule {} is declared twice", r.id)],
                    clean: false,
                    unavailable: None,
                };
            }
        }
        m
    };

    // Every rule in 1..=10 must be declared. A standard with a missing rule is not
    // a standard with a gap in the gate; it is a gate that was never asked.
    for id in 1..=10usize {
        if !declared.contains_key(&id) {
            return Report {
                lines: vec![format!("rule {id} is not declared in tetanus.toml")],
                clean: false,
                unavailable: None,
            };
        }
    }

    let verdict = match collect(root, &file) {
        Ok(v) => v,
        Err(e) => {
            return Report {
                lines: vec![format!("could not walk the source tree: {e}")],
                clean: false,
                unavailable: Some(e),
            };
        }
    };

    let mut lines = Vec::new();
    let mut clean = verdict.is_clean();

    for f in &verdict.parse_failures {
        lines.push(format!("  {f}"));
    }
    if !verdict.parse_failures.is_empty() {
        lines.push(format!(
            "\n{} file(s) did not parse. Reported as failures rather than skipped: an \
             unparsed file is an unchecked file, and an unchecked file must not read \
             as a clean one.",
            verdict.parse_failures.len()
        ));
    }

    // Declared sites, keyed by the pair the ratchet compares on.
    let mut declared_sites: BTreeMap<(usize, String), &Baseline> = BTreeMap::new();
    for b in &file.baseline {
        let key = (b.rule, b.location.clone());
        if declared_sites.insert(key, b).is_some() {
            clean = false;
            lines.push(format!("  baseline declares {} twice", b.location));
            continue;
        }
        match declared.get(&b.rule) {
            None => {
                clean = false;
                lines.push(format!(
                    "  {} cites rule {}, which is not declared",
                    b.location, b.rule
                ));
            }
            Some(r) => {
                if r.disposition == Disposition::Reported {
                    clean = false;
                    lines.push(format!(
                        "  {} is a rule-{} site, but rule {} is `reported`, not `gated`, so \
                         it must not be baselined",
                        b.location, b.rule, b.rule
                    ));
                }
                if PLACEHOLDER_MARKERS.iter().any(|m| b.reason.contains(m)) {
                    clean = false;
                    lines.push(format!(
                        "  {} has a placeholder reason. The gate cannot tell a real \
                         justification from a well-formed one; only a person can.",
                        b.location
                    ));
                }
            }
        }
    }

    // A `Review`/`Mixed` rule must name its artefact; a `Mechanical` one must not,
    // because that would claim a human pass the gate never asked for.
    for r in file.rules.iter() {
        let artifact = r.review_artifact.as_deref().unwrap_or("").trim();
        match r.check {
            Check::Mechanical if !artifact.is_empty() => {
                clean = false;
                lines.push(format!(
                    "  rule {} is `mechanical` but names a review_artifact",
                    r.id
                ));
            }
            Check::Review | Check::Mixed if artifact.is_empty() => {
                clean = false;
                lines.push(format!(
                    "  rule {} is `{:?}` with no review_artifact: a clause the gate cannot \
                     decide, recorded nowhere",
                    r.id, r.check
                ));
            }
            _ => {}
        }
    }

    let gated = |r: &Rule| r.disposition == Disposition::Gated;

    // Direction one: a finding with no baseline entry. New violations.
    for f in verdict.findings.iter().filter(|f| gated(declared[&f.rule])) {
        if !declared_sites.contains_key(&(f.rule, f.location.clone())) {
            clean = false;
            lines.push(format!(
                "  new: rule {} at {}: {}",
                f.rule, f.location, f.detail
            ));
        }
    }

    // Direction two: an entry with no finding. A justification that outlived its
    // code, which is how a standard degrades into a comment.
    let found_keys: BTreeSet<(usize, String)> = verdict
        .findings
        .iter()
        .map(|f| (f.rule, f.location.clone()))
        .collect();
    for (key, b) in &declared_sites {
        if !found_keys.contains(key) {
            clean = false;
            lines.push(format!(
                "  stale: rule {} at {} is baselined but no longer found",
                key.0, b.location
            ));
        }
    }

    // Reported rules: visible, never baselined.
    let mut reported_lines = Vec::new();
    for r in file.rules.iter().filter(|r| !gated(r)) {
        let n = verdict.findings.iter().filter(|f| f.rule == r.id).count();
        reported_lines.push(format!(
            "  rule {:>2}: {:>4} site(s) reported, not gated -- {}",
            r.id, n, r.title
        ));
    }

    lines.push(String::new());
    lines.push(format!(
        "standard: {} ({})",
        file.meta.standard, file.meta.source
    ));
    for line in file.meta.note.lines() {
        lines.push(format!("note: {line}"));
    }
    lines.push(String::new());
    lines.push(format!(
        "{} file(s) parsed, {} finding(s), {} baselined.",
        verdict.scanned,
        verdict.findings.len(),
        file.baseline.len()
    ));
    if !reported_lines.is_empty() {
        lines.push(
            "\nRules reported rather than gated. Visible so the number cannot quietly\n\
             grow; ungated because baselining them would record ~1000 justifications\n\
             saying the same thing, and the count would be the only thing ever read."
                .to_string(),
        );
        lines.extend(reported_lines);
    }

    Report {
        lines,
        clean,
        unavailable: None,
    }
}

/// Gate entry point.
pub fn gate(name: &str, root: &Path, started: Instant, dry_run: bool) -> GateResult {
    if dry_run {
        return GateResult {
            name: name.to_string(),
            outcome: Outcome::Passed,
            duration: Duration::ZERO,
            detail: Some(
                "(dry run) parses every .rs under scan_roots and diffs the \
                         findings against tetanus.toml"
                    .to_string(),
            ),
        };
    }

    let report = analyse(root);
    let detail = report.lines.join("\n");
    let outcome = if let Some(_reason) = report.unavailable {
        Outcome::Unavailable
    } else if report.clean {
        Outcome::Passed
    } else {
        Outcome::Failed
    };
    GateResult {
        name: name.to_string(),
        outcome,
        duration: started.elapsed(),
        detail: Some(detail),
    }
}

/// `cargo xtask tetanus` -- print findings as `rule<TAB>location<TAB>detail`, so
/// `bless` and a human read the same thing.
pub fn print_findings(root: &Path) -> Result<(), String> {
    let file = load(root)?;
    let verdict = collect(root, &file)?;
    for f in &verdict.findings {
        println!("{}\t{}\t{}", f.rule, f.location, f.detail);
    }
    for f in &verdict.parse_failures {
        println!("!\t{f}\t");
    }
    Ok(())
}

/// `cargo xtask bless` -- write a baseline entry for every finding that lacks one.
///
/// Refuses on an unparseable file, and refuses on an empty scan. Both refusals
/// exist for the same reason: a checker that has stopped walking the tree produces
/// a finding set that is *smaller*, and writing a baseline from it would delete the
/// record of the sites it stopped checking. That is not a refactor of the baseline;
/// it is the failure the baseline exists to catch.
pub fn bless(root: &Path) -> Result<(), String> {
    let file = load(root)?;
    let verdict = collect(root, &file)?;

    if !verdict.parse_failures.is_empty() {
        return Err(format!(
            "{} file(s) did not parse, so the finding set is not trustworthy:\n  {}\nRefusing to \
             write a baseline from a partial scan.",
            verdict.parse_failures.len(),
            verdict.parse_failures.join("\n  ")
        ));
    }
    if verdict.findings.is_empty() {
        return Err(
            "the scan found nothing. That is indistinguishable from a checker that stopped \
             walking the tree, so refusing to write it."
                .to_string(),
        );
    }

    let have: BTreeSet<(usize, String)> = file
        .baseline
        .iter()
        .map(|b| (b.rule, b.location.clone()))
        .collect();
    let by_id: BTreeMap<usize, &Rule> = file.rules.iter().map(|r| (r.id, r)).collect();

    let mut added = 0usize;
    let mut out = String::new();
    for f in &verdict.findings {
        let key = (f.rule, f.location.clone());
        if have.contains(&key) {
            continue;
        }
        let Some(rule) = by_id.get(&f.rule) else {
            continue;
        };
        if rule.disposition != Disposition::Gated {
            continue;
        }
        let reason = PLACEHOLDER_MARKERS.first().map_or_else(
            || "TODO: justify this site. The gate will reject the placeholder.".to_string(),
            |m| format!("{m}: justify this site."),
        );
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = write!(
            out,
            "\n[[baseline]]\nrule = {}\nlocation = \"{}\"\nreason = \"{}\"\n",
            f.rule,
            esc(&f.location),
            esc(&reason)
        );
        added += 1;
    }

    if added == 0 {
        println!("baseline already covers every finding; nothing to write");
        return Ok(());
    }
    let path = root.join("tetanus.toml");
    let mut text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.push_str(&out);
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {added} baseline entr(ies), each with a TODO reason the gate will reject");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("tetanus-test-{}-{n}", std::process::id()));
            fs::create_dir_all(&p).expect("scratch");
            Self(p)
        }
        fn file(&self, body: &str) -> String {
            let p = self.0.join("x.rs");
            fs::write(&p, body).expect("write");
            p.to_string_lossy().into_owned()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn rules_of(path: &str) -> Vec<usize> {
        let text = fs::read_to_string(path).expect("read");
        scan("x.rs", &syn::parse_file(&text).expect("parse"))
            .iter()
            .map(|f| f.rule)
            .collect()
    }

    #[test]
    fn rule_1_finds_a_direct_call() {
        let s = Scratch::new();
        let p = s.file("pub fn down(n: u32) -> u32 { if n == 0 { 0 } else { down(n - 1) } }\n");
        assert!(rules_of(&p).contains(&1), "expected rule 1: {p:?}");
    }

    /// The regression that produced four false positives: a closure that *mentions*
    /// the enclosing function is not a call to it.
    #[test]
    fn rule_1_ignores_a_bare_reference() {
        let s = Scratch::new();
        let p = s.file("pub fn f() -> fn() { f }\n");
        assert!(!rules_of(&p).contains(&1), "a bare path is not a call");
    }

    #[test]
    fn rule_1_ignores_a_sibling_with_a_shared_prefix() {
        let s = Scratch::new();
        let p = s.file("pub fn f() -> u32 { helper() }\nfn helper() -> u32 { 0 }\n");
        assert!(!rules_of(&p).contains(&1), "helper is not f");
    }

    #[test]
    fn rule_2_finds_a_loop_with_no_exit() {
        let s = Scratch::new();
        let p = s.file("pub fn f() { loop { work(); } }\n");
        assert!(rules_of(&p).contains(&2));
    }

    /// The regression from the other direction: a `break` under an `if` is not an
    /// exit, so this loop is still unbounded.
    #[test]
    fn rule_2_rejects_a_conditional_break_as_an_exit() {
        let s = Scratch::new();
        let p =
            s.file("pub fn f(mut n: u32) { while n > 0 { if n % 2 == 0 { break; } n -= 1; } }\n");
        assert!(
            rules_of(&p).contains(&2),
            "a conditional break does not terminate the loop"
        );
    }

    #[test]
    fn rule_2_accepts_an_unconditional_break() {
        let s = Scratch::new();
        let p = s.file("pub fn f(mut n: u32) { while n > 0 { break; } }\n");
        assert!(!rules_of(&p).contains(&2));
    }

    #[test]
    fn rule_2_ignores_a_nested_loops_exit() {
        let s = Scratch::new();
        let p = s.file("pub fn f() { while a() { loop { break; } } }\n");
        assert!(
            rules_of(&p).contains(&2),
            "the inner break exits the inner loop"
        );
    }

    /// Rule 4 counts statements, so comments do not push a function over the limit.
    #[test]
    fn rule_4_ignores_comments() {
        let s = Scratch::new();
        let body: String = std::iter::repeat("// a line of commentary\n    ")
            .take(300)
            .collect::<String>();
        let p = s.file(&format!(
            "pub fn f() {{\n    {body}let x = 1;\n    let _ = x;\n}}\n"
        ));
        assert!(!rules_of(&p).contains(&4), "comments are not statements");
    }

    #[test]
    fn rule_4_counts_statements() {
        let s = Scratch::new();
        let stmts: String = (0..70)
            .map(|i| format!("let v{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n    ");
        let p = s.file(&format!("pub fn f() {{\n    {stmts}\n}}\n"));
        assert!(rules_of(&p).contains(&4));
    }

    #[test]
    fn rule_8_finds_cfg_attributes() {
        let s = Scratch::new();
        let p = s.file("#[cfg(feature = \"redis\")]\npub fn f() {}\n");
        assert!(rules_of(&p).contains(&8));
    }

    #[test]
    fn rule_10_finds_a_suppression() {
        let s = Scratch::new();
        let p = s.file("#[allow(clippy::too_many_lines)]\npub fn f() {}\n");
        assert!(rules_of(&p).contains(&10));
    }

    /// Rules 6 and 7 find nothing in this crate. That is only acceptable if the
    /// checks work, so each has a case that must fire. A gated rule that finds
    /// zero sites because nothing looks is the same defect as an unimplemented one:
    /// it reads as a pass.
    #[test]
    fn rule_6_finds_static_state() {
        let s = Scratch::new();
        let p = s.file("static COUNTER: AtomicUsize = AtomicUsize::new(0);\npub fn f() {}\n");
        assert!(
            rules_of(&p).contains(&6),
            "a `static` is exactly what rule 6 is about"
        );
    }

    #[test]
    fn rule_7_finds_a_diverging_branch_beside_a_yielding_one() {
        let s = Scratch::new();
        let p = s.file("pub fn f(b: bool) -> u32 { if b { return 1; } else { 2 } }\n");
        assert!(
            rules_of(&p).contains(&7),
            "a `return` beside a value is check-then-act"
        );
    }

    #[test]
    fn rule_7_accepts_an_if_with_no_diverging_branch() {
        let s = Scratch::new();
        let p = s.file("pub fn f(b: bool) -> u32 { if b { 1 } else { 2 } }\n");
        assert!(!rules_of(&p).contains(&7));
    }

    /// The false positive this rule is deliberately allowed to miss. Documented as
    /// a test so the omission stays a decision rather than becoming a bug.
    #[test]
    fn rule_7_ignores_a_bare_early_return() {
        let s = Scratch::new();
        let p = s.file("pub fn f(b: bool) -> u32 { if b { return 1; } 2 }\n");
        assert!(
            !rules_of(&p).contains(&7),
            "an implicit else is out of scope by design"
        );
    }

    /// The count this crate actually has, as a floor. If rule 3 starts finding
    /// nothing, the detector broke -- and a zero would otherwise read as "this
    /// crate never allocates", which is false in several hundred places.
    #[test]
    fn rule_3_finds_an_allocation() {
        let s = Scratch::new();
        let p = s.file("pub fn f() -> String { format!(\"{}\", 1) }\n");
        assert!(rules_of(&p).contains(&3));
    }

    #[test]
    fn rule_9_finds_a_bare_trait_object() {
        let s = Scratch::new();
        let p = s.file("pub fn f(x: &dyn Fn()) { let _ = x; }\n");
        assert!(rules_of(&p).contains(&9));
    }

    /// An unparseable file must be a failure, never a silent absence.
    #[test]
    fn a_parse_failure_is_not_a_pass() {
        let s = Scratch::new();
        let dir = &s.0;
        fs::write(dir.join("good.rs"), "pub fn f() {}\n").unwrap();
        fs::write(dir.join("bad.rs"), "pub fn ((( {\n").unwrap();
        let v = collect(
            Path::new("/nonexistent"),
            &file_for(&[dir.to_string_lossy().into_owned()]),
        )
        .expect("walk");
        assert!(
            !v.is_clean(),
            "a file that did not parse is an unchecked file"
        );
        assert_eq!(v.parse_failures.len(), 1);
    }

    /// The refusal that matters: an empty scan cannot silently empty a baseline.
    #[test]
    fn bless_refuses_an_empty_scan() {
        let s = Scratch::new();
        let dir = &s.0;
        fs::write(dir.join("clean.rs"), "pub fn f() {}\n").unwrap();
        // No rules to trip, so the finding set is empty.
        let err = bless_at(
            Path::new("/nonexistent"),
            &file_for(&[dir.to_string_lossy().into_owned()]),
        )
        .expect_err("must refuse");
        assert!(err.contains("found nothing"), "{err}");
    }

    fn file_for(roots: &[String]) -> File {
        File {
            meta: Meta {
                standard: "test".into(),
                source: "test".into(),
                note: String::new(),
                scan_roots: roots.to_vec(),
            },
            rules: (1..=10)
                .map(|id| Rule {
                    id,
                    title: "t".into(),
                    check: Check::Mechanical,
                    disposition: Disposition::Gated,
                    review_artifact: None,
                })
                .collect(),
            baseline: Vec::new(),
        }
    }

    /// `bless` against an in-memory contract, so the test does not need a repo.
    fn bless_at(root: &Path, file: &File) -> Result<(), String> {
        let verdict = collect(root, file)?;
        if !verdict.parse_failures.is_empty() {
            return Err("parse failures".into());
        }
        if verdict.findings.is_empty() {
            return Err("the scan found nothing. Refusing.".into());
        }
        Ok(())
    }
}
