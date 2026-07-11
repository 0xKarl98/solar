//! End-to-end Solar LSP performance harness.

// This standalone harness intentionally manages external binaries and fixture files directly.
#![allow(clippy::disallowed_methods)]

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::{path::PathBuf, time::Duration};

mod fixture;
mod process;
mod protocol;
mod report;
mod runner;
mod scenario;

#[derive(Parser)]
#[command(name = "solar-lsp-bench", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compare two pre-built Solar binaries with identical LSP workloads.
    Compare {
        /// Baseline Solar binary.
        #[arg(long)]
        baseline: PathBuf,

        /// Candidate Solar binary.
        #[arg(long)]
        candidate: PathBuf,

        /// Scenario to run, or `all` for every core scenario.
        #[arg(long, value_enum, default_value = "all")]
        scenario: scenario::Selection,

        /// Independent process runs per binary and scenario.
        #[arg(long, default_value_t = 10)]
        repeat: usize,

        /// Per-operation LSP timeout.
        #[arg(long, default_value_t = 10)]
        timeout_secs: u64,

        /// Directory for JSON samples and summaries.
        #[arg(long, default_value = "target/lsp-bench/latest")]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    let Cli { command } = Cli::parse();
    match command {
        Command::Compare { baseline, candidate, scenario, repeat, timeout_secs, output } => {
            let outcome = runner::compare(runner::CompareOptions {
                baseline,
                candidate,
                selection: scenario,
                repeat,
                timeout: Duration::from_secs(timeout_secs),
                output: output.clone(),
            })?;
            print!("{}", report::terminal(&outcome.summary));
            println!("Reports: {}", output.display());
            if outcome.failed_runs != 0 {
                bail!("{} benchmark run(s) failed", outcome.failed_runs)
            }
        }
    }
    Ok(())
}
