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

        /// Clean checkout of the pinned Solady revision.
        #[arg(long)]
        project: PathBuf,

        /// Absolute path to a Forge binary that supports `forge lint`.
        #[arg(long)]
        forge: PathBuf,

        /// Independent user-session runs per binary.
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
        Command::Compare { baseline, candidate, project, forge, repeat, timeout_secs, output } => {
            let outcome = runner::compare(runner::CompareOptions {
                baseline,
                candidate,
                project,
                forge,
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
