//! Gate runner and contract validator for `theSix`.
//!
//! The contract (`theSix.toml`) declares the gates; this binary executes them.
//! Nothing here decides what "verified" means — it only refuses to claim
//! success for a gate it did not actually run.

mod contract;
mod gates;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use contract::Contract;
use gates::{Outcome, RunOptions, Toolchain};
use style::*;

const ROOT: &str = "theSix.toml";

/// Colour helpers. Hand-rolled rather than pulled from a crate: three escape
/// sequences do not justify a dependency in a dev-only binary.
mod style {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";

    pub fn ok() -> String {
        format!("{GREEN}pass{RESET}")
    }
    pub fn fail() -> String {
        format!("{RED}FAIL{RESET}")
    }
    pub fn skip() -> String {
        format!("{YELLOW}skip{RESET}")
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "xtask",
    about = "Verification gates for theSix, driven by theSix.toml",
    long_about = "Runs the architectural contract's verification gates in the order the contract \
                  declares them.\n\nEvery gate is executed for real; a gate whose tooling is not \
                  installed is reported as unavailable rather than quietly counted as passing."
)]
struct Cli {
    /// Repository root. Defaults to the current directory.
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    /// Print every command instead of running it.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Stream output even for gates that pass.
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the default pass: every gate the contract marks as mandatory.
    Gates,
    /// Run one gate by name, or `all` for every gate including deferred ones.
    Run {
        /// Gate name from the contract.
        name: String,
    },
    /// Validate the contract and cross-check it against this runner.
    Contract,
    /// List gates in contract order.
    List,
    /// List gates excluded from the default pass, with the reason.
    Deferred,
    /// Print the exact argv for a gate without running it.
    Show {
        /// Gate name from the contract.
        name: String,
    },
    /// Report detected build accelerations.
    Toolchain,
    /// Print the test-target-to-layer mapping the contract declares.
    Layers,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = cli.root.clone();
    let contract_path = root.join(ROOT);

    let contract = match Contract::load(&contract_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{RED}contract error{RESET}\n{e}");
            return ExitCode::from(2);
        }
    };

    // The contract must also describe this checkout: a gate pointing at a
    // test target that does not exist is coverage that only exists in review.
    if let Err(e) = contract.validate_targets(&root) {
        eprintln!("{RED}contract/repo mismatch{RESET}\n{e}");
        return ExitCode::from(2);
    }

    let toolchain = Toolchain::probe();
    let opts = RunOptions {
        root: root.clone(),
        toolchain: toolchain.clone(),
        dry_run: cli.dry_run,
        verbose: cli.verbose,
    };

    match cli.command.unwrap_or(Command::Gates) {
        Command::Contract => {
            print_contract(&contract);
            ExitCode::SUCCESS
        }
        Command::Toolchain => {
            println!("{}", toolchain.summary());
            if let Some(s) = &toolchain.sccache {
                println!("  sccache   {}", s.display());
            }
            if let Some(l) = &toolchain.linker {
                println!("  linker    {}", l.display());
            }
            if let Some(m) = &toolchain.mold {
                println!("  mold      {}", m.display());
            }
            ExitCode::SUCCESS
        }
        Command::List => {
            print_gates(&contract, false);
            ExitCode::SUCCESS
        }
        Command::Deferred => {
            print_gates(&contract, true);
            ExitCode::SUCCESS
        }
        Command::Layers => {
            println!("{BOLD}test target layers{RESET}");
            for (layer, targets) in &contract.layers {
                println!("  {CYAN}{layer:<16}{RESET} {}", targets.join(", "));
            }
            ExitCode::SUCCESS
        }
        Command::Show { name } => match contract.gate(&name) {
            Some(g) => {
                println!("{}", gates::argv_for(g, &toolchain).join(" "));
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("{RED}no gate named {name:?}{RESET}");
                ExitCode::from(2)
            }
        },
        Command::Run { name } => {
            let selected = match gates::resolve(&contract, Some(&name)) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{RED}{e}{RESET}");
                    return ExitCode::from(2);
                }
            };
            run_all(&selected, &opts, &toolchain)
        }
        Command::Gates => {
            let selected = gates::resolve(&contract, None).expect("default pass always resolves");
            run_all(&selected, &opts, &toolchain)
        }
    }
}

fn run_all(selected: &[&contract::Gate], opts: &RunOptions, tc: &Toolchain) -> ExitCode {
    let started = Instant::now();
    println!(
        "{BOLD}theSix gates{RESET} {DIM}(contract {}, {} gates){RESET}",
        opts.root.join(ROOT).display(),
        selected.len()
    );
    println!("{DIM}{}{RESET}", tc.summary());

    let mut results = Vec::new();
    for (i, gate) in selected.iter().enumerate() {
        let n = i + 1;
        println!(
            "\n{BOLD}[{n:>2}/{}] {}{RESET} {DIM}{}{RESET}",
            selected.len(),
            gate.name,
            gate.kind.as_str()
        );
        let r = gates::run(gate, opts);
        let (label, detail) = match r.outcome {
            Outcome::Passed => (style::ok(), r.detail.clone()),
            Outcome::Failed => (style::fail(), r.detail.clone()),
            Outcome::Unavailable => (style::skip(), r.detail.clone()),
        };
        println!("      {label} {DIM}{:.2?}{RESET}", r.duration);
        if let Some(d) = detail {
            println!("      {DIM}{d}{RESET}");
        }
        results.push(r);
    }

    // Summary. An unavailable gate is reported as a distinct outcome: a matrix
    // that silently drops a gate is how "29 tests prove the invariants" became
    // a claim nobody could reproduce.
    let passed = results
        .iter()
        .filter(|r| r.outcome == Outcome::Passed)
        .count();
    let failed = results
        .iter()
        .filter(|r| r.outcome == Outcome::Failed)
        .count();
    let unavail = results
        .iter()
        .filter(|r| r.outcome == Outcome::Unavailable)
        .count();

    println!("\n{BOLD}summary{RESET}");
    for r in &results {
        let label = match r.outcome {
            Outcome::Passed => style::ok(),
            Outcome::Failed => style::fail(),
            Outcome::Unavailable => style::skip(),
        };
        println!("  {label} {:<16} {DIM}{:>7.2?}{RESET}", r.name, r.duration);
    }
    println!(
        "\n{BOLD}{passed} passed{RESET}, {failed} failed, {unavail} unavailable in {:.2?}",
        started.elapsed()
    );

    if failed > 0 {
        ExitCode::FAILURE
    } else if unavail > 0 {
        // Distinct exit code: "I could not verify this" must not look like
        // "I verified this".
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    }
}

fn print_contract(c: &Contract) {
    println!("{BOLD}contract{RESET} {}", c.path.display());
    println!("  {CYAN}version{RESET} {}", c.version);
    println!("  {CYAN}gates{RESET}   {}", c.gates.len());
    println!("  {CYAN}layers{RESET}  {}", c.layers.len());
    for (label, count) in c.declared_case_counts() {
        println!("  {CYAN}{label:<26}{RESET} {count}");
    }
    println!("\n{BOLD}mandatory verification flags{RESET}");
    for (flag, value) in &c.required_booleans {
        let mark = if *value {
            format!("{GREEN}true{RESET}")
        } else {
            format!("{RED}false{RESET}")
        };
        println!("  {flag:<32} {mark}");
    }
    println!("\n{GREEN}contract is internally consistent{RESET}");
    println!(
        "{DIM}gate order matches declaration order; every layer target is claimed by a gate; \
              fault taxonomy matches the crate{RESET}"
    );
}

fn print_gates(c: &Contract, deferred_only: bool) {
    let list = if deferred_only {
        c.deferred()
    } else {
        c.default_pass()
    };
    println!("{BOLD}gates{RESET} {DIM}({} shown){RESET}", list.len());
    for g in list {
        let marker = if deferred_only {
            format!("{YELLOW}deferred{RESET}")
        } else {
            format!("{CYAN}required{RESET}")
        };
        println!("  {:<16} {:<8} {marker}", g.name, g.kind.as_str());
        println!("  {:<16} {DIM}{}{RESET}", "", g.description);
    }
}
