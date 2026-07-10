use crate::{
    process::{Observations, ProcessMetrics},
    scenario::Scenario,
};
use anyhow::Result;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

pub(crate) const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BinaryMetadata {
    pub(crate) label: &'static str,
    pub(crate) path: PathBuf,
    pub(crate) version: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct HarnessConfig {
    pub(crate) repeat: usize,
    pub(crate) timeout_ms: u64,
    pub(crate) scenarios: Vec<Scenario>,
    pub(crate) fixture_file_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Environment {
    pub(crate) os: &'static str,
    pub(crate) architecture: &'static str,
    pub(crate) logical_cpus: usize,
}

impl Environment {
    pub(crate) fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RunStatus {
    Ok,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RunSample {
    pub(crate) label: &'static str,
    pub(crate) binary: PathBuf,
    pub(crate) scenario: Scenario,
    pub(crate) repetition: usize,
    pub(crate) status: RunStatus,
    pub(crate) scenario_wall_ms: Option<f64>,
    pub(crate) analysis_latencies_ms: Vec<f64>,
    pub(crate) process: Option<ProcessMetrics>,
    pub(crate) observations: Observations,
    pub(crate) error: Option<String>,
}

impl RunSample {
    pub(crate) fn succeeded(&self) -> bool {
        matches!(self.status, RunStatus::Ok)
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ScenarioSummary {
    pub(crate) label: &'static str,
    pub(crate) scenario: Scenario,
    pub(crate) successful_runs: usize,
    pub(crate) failed_runs: usize,
    pub(crate) scenario_wall_ms: Stats,
    pub(crate) analysis_latency_ms: Stats,
    pub(crate) request_latency_ms: Stats,
    pub(crate) process_cpu_ms: Stats,
    pub(crate) peak_rss_mib: Stats,
    pub(crate) diagnostic_publications: Stats,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SummaryReport {
    pub(crate) schema_version: u32,
    pub(crate) config: HarnessConfig,
    pub(crate) environment: Environment,
    pub(crate) binaries: Vec<BinaryMetadata>,
    pub(crate) summaries: Vec<ScenarioSummary>,
}

#[derive(Serialize)]
struct SamplesReport<'a> {
    schema_version: u32,
    samples: &'a [RunSample],
}

pub(crate) fn summarize(
    config: HarnessConfig,
    binaries: Vec<BinaryMetadata>,
    samples: &[RunSample],
) -> SummaryReport {
    let mut groups = BTreeMap::<(&'static str, Scenario), Vec<&RunSample>>::new();
    for sample in samples {
        groups.entry((sample.label, sample.scenario)).or_default().push(sample);
    }

    let summaries = groups
        .into_iter()
        .map(|((label, scenario), samples)| {
            let successful =
                samples.iter().copied().filter(|sample| sample.succeeded()).collect::<Vec<_>>();
            let scenario_wall =
                successful.iter().filter_map(|sample| sample.scenario_wall_ms).collect::<Vec<_>>();
            let analysis_latency = successful
                .iter()
                .flat_map(|sample| sample.analysis_latencies_ms.iter().copied())
                .collect::<Vec<_>>();
            let request_latency = successful
                .iter()
                .flat_map(|sample| {
                    sample.observations.requests.iter().map(|request| request.elapsed_ms)
                })
                .collect::<Vec<_>>();
            let process_cpu = successful
                .iter()
                .filter_map(|sample| {
                    let process = sample.process.as_ref()?;
                    Some(process.user_cpu_ms? + process.system_cpu_ms?)
                })
                .collect::<Vec<_>>();
            let peak_rss = successful
                .iter()
                .filter_map(|sample| sample.process.as_ref()?.peak_rss_mib)
                .collect::<Vec<_>>();
            let diagnostic_publications = successful
                .iter()
                .map(|sample| sample.observations.diagnostic_publications as f64)
                .collect::<Vec<_>>();
            ScenarioSummary {
                label,
                scenario,
                successful_runs: successful.len(),
                failed_runs: samples.len() - successful.len(),
                scenario_wall_ms: Stats::new(&scenario_wall),
                analysis_latency_ms: Stats::new(&analysis_latency),
                request_latency_ms: Stats::new(&request_latency),
                process_cpu_ms: Stats::new(&process_cpu),
                peak_rss_mib: Stats::new(&peak_rss),
                diagnostic_publications: Stats::new(&diagnostic_publications),
            }
        })
        .collect();

    SummaryReport {
        schema_version: SCHEMA_VERSION,
        config,
        environment: Environment::current(),
        binaries,
        summaries,
    }
}

pub(crate) fn write_reports(
    output: &Path,
    summary: &SummaryReport,
    samples: &[RunSample],
) -> Result<()> {
    fs::create_dir_all(output)?;
    fs::write(output.join("summary.json"), serde_json::to_vec_pretty(summary)?)?;
    fs::write(
        output.join("samples.json"),
        serde_json::to_vec_pretty(&SamplesReport { schema_version: SCHEMA_VERSION, samples })?,
    )?;
    fs::write(output.join("summary.md"), markdown(summary))?;
    Ok(())
}

fn markdown(report: &SummaryReport) -> String {
    let mut output = String::from(
        "# Solar LSP benchmark\n\n| Binary | Scenario | Runs | Wall p50 | Wall p95 | Analysis p95 | CPU p50 | Peak RSS |\n|---|---|---:|---:|---:|---:|---:|---:|\n",
    );
    for summary in &report.summaries {
        let _ = writeln!(
            output,
            "| {} | {} | {}/{} | {:.2} ms | {:.2} ms | {:.2} ms | {:.2} ms | {:.2} MiB |",
            summary.label,
            summary.scenario.name(),
            summary.successful_runs,
            summary.successful_runs + summary.failed_runs,
            summary.scenario_wall_ms.p50,
            summary.scenario_wall_ms.p95,
            summary.analysis_latency_ms.p95,
            summary.process_cpu_ms.p50,
            summary.peak_rss_mib.max,
        );
    }
    output
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Stats {
    pub(crate) count: usize,
    pub(crate) mean: f64,
    pub(crate) p50: f64,
    pub(crate) p95: f64,
    pub(crate) max: f64,
}

impl Stats {
    pub(crate) fn new(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self { count: 0, mean: 0.0, p50: 0.0, p95: 0.0, max: 0.0 };
        }

        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        Self {
            count: sorted.len(),
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            p50: percentile(&sorted, 0.50),
            p95: percentile(&sorted, 0.95),
            max: *sorted.last().unwrap(),
        }
    }
}

fn percentile(sorted: &[f64], ratio: f64) -> f64 {
    let rank = (sorted.len() as f64 * ratio).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::Observations;
    use std::path::PathBuf;

    #[test]
    fn summary_uses_nearest_rank_percentiles() {
        let stats = Stats::new(&[1.0, 2.0, 3.0, 4.0, 100.0]);

        assert_eq!(stats.count, 5);
        assert_eq!(stats.p50, 3.0);
        assert_eq!(stats.p95, 100.0);
        assert_eq!(stats.max, 100.0);
    }

    #[test]
    fn summaries_keep_failed_runs_out_of_latency_statistics() {
        let samples = [
            RunSample {
                label: "baseline",
                binary: PathBuf::from("solar"),
                scenario: Scenario::Startup,
                repetition: 0,
                status: RunStatus::Ok,
                scenario_wall_ms: Some(10.0),
                analysis_latencies_ms: vec![5.0],
                process: None,
                observations: Observations::default(),
                error: None,
            },
            RunSample {
                label: "baseline",
                binary: PathBuf::from("solar"),
                scenario: Scenario::Startup,
                repetition: 1,
                status: RunStatus::Failed,
                scenario_wall_ms: None,
                analysis_latencies_ms: Vec::new(),
                process: None,
                observations: Observations::default(),
                error: Some("timeout".into()),
            },
        ];
        let report = summarize(
            HarnessConfig {
                repeat: 2,
                timeout_ms: 1_000,
                scenarios: vec![Scenario::Startup],
                fixture_file_count: 184,
            },
            Vec::new(),
            &samples,
        );

        assert_eq!(report.summaries[0].successful_runs, 1);
        assert_eq!(report.summaries[0].failed_runs, 1);
        assert_eq!(report.summaries[0].scenario_wall_ms.p50, 10.0);
    }
}
