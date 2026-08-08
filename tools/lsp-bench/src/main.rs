//! Cross-server Solidity LSP benchmark command line tool.

#![allow(clippy::disallowed_methods)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{collections::BTreeSet, path::PathBuf, time::Duration};

mod config;
mod fixture;
mod lifecycle;
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
    /// Fetch pinned sources and build or install benchmark dependencies.
    Prepare {
        /// Versioned benchmark manifest.
        #[arg(long, default_value = "tools/lsp-bench/benchmark.yaml")]
        config: PathBuf,
        /// Restrict preparation to these server ids (repeatable).
        #[arg(long = "server", value_name = "ID")]
        servers: Vec<String>,
        /// Restrict preparation to these fixture ids (repeatable).
        #[arg(long = "fixture", value_name = "ID")]
        fixtures: Vec<String>,
    },
    /// Audit manifests, artifacts, fixtures, and the execution environment.
    Doctor {
        /// Versioned benchmark manifest.
        #[arg(long, default_value = "tools/lsp-bench/benchmark.yaml")]
        config: PathBuf,
        /// Require the authoritative Linux/cgroup environment and a clean tree.
        #[arg(long)]
        publish: bool,
    },
    /// Run all selected servers through the same manifest-defined workloads.
    Run {
        /// Versioned benchmark manifest.
        #[arg(long, default_value = "tools/lsp-bench/benchmark.yaml")]
        config: PathBuf,
        /// Independent process runs per server and workload.
        #[arg(long, default_value_t = 0)]
        repeat: usize,
        /// Per-operation and shutdown timeout.
        #[arg(long, default_value_t = 0)]
        timeout_secs: u64,
        /// Sampling profile from the benchmark manifest.
        #[arg(long, default_value = "default")]
        profile: String,
        /// Directory for raw samples and summaries.
        #[arg(long, default_value = "target/lsp-bench/latest")]
        output: PathBuf,
        /// Restrict execution to these server ids (repeatable).
        #[arg(long = "server", value_name = "ID")]
        servers: Vec<String>,
        /// Restrict execution to these workload ids (repeatable).
        #[arg(long = "workload", value_name = "ID")]
        workloads: Vec<String>,
        /// Write all reports but exit successfully when samples fail.
        #[arg(long)]
        allow_failures: bool,
    },
    /// Regenerate Markdown from an existing summary JSON.
    Report {
        /// Summary JSON produced by `run`.
        #[arg(long, default_value = "target/lsp-bench/latest/summary.json")]
        input: PathBuf,
        /// Markdown report destination.
        #[arg(long, default_value = "COMPARISON.md")]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    let Cli { command } = Cli::parse();
    match command {
        Command::Prepare { config, servers, fixtures } => {
            let report = lifecycle::prepare(lifecycle::PrepareOptions {
                config,
                servers: servers.into_iter().collect(),
                fixtures: fixtures.into_iter().collect(),
            })?;
            print!("{}", lifecycle::render_doctor(&report));
        }
        Command::Doctor { config, publish } => {
            let report = lifecycle::doctor(lifecycle::DoctorOptions { config, publish })?;
            print!("{}", lifecycle::render_doctor(&report));
        }
        Command::Run {
            config,
            repeat,
            timeout_secs,
            profile,
            output,
            servers,
            workloads,
            allow_failures,
        } => {
            let outcome = runner::run(runner::RunOptions {
                config,
                repeat,
                timeout: Duration::from_secs(timeout_secs),
                profile,
                output: output.clone(),
                servers: servers.into_iter().collect::<BTreeSet<_>>(),
                workloads: workloads.into_iter().collect::<BTreeSet<_>>(),
            })?;
            print!("{}", report::terminal(&outcome.summary));
            println!("Reports: {}", output.display());
            if outcome.failed_runs != 0 {
                eprintln!(
                    "{} benchmark sample(s) were excluded from performance statistics",
                    outcome.failed_runs
                );
                if !allow_failures {
                    anyhow::bail!("benchmark contains failing samples; reports were retained")
                }
            }
        }
        Command::Report { input, output } => {
            report::regenerate_markdown(&input, &output)?;
            println!("Report: {}", output.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_exposes_prepare_doctor_run_and_report() {
        for command in ["prepare", "doctor", "run", "report"] {
            assert!(Cli::try_parse_from(["solar-lsp-bench", command]).is_ok(), "{command}");
        }
    }
}
