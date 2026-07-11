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

pub(crate) fn terminal(report: &SummaryReport) -> String {
    let mut output = String::from(
        "Solar LSP benchmark results\n\
         Lower is better; change compares candidate with baseline.\n\n\
         Scenario                Metric                     Baseline      Candidate      Change\n\
         ----------------------  ----------------------  -----------  -------------  ----------\n",
    );
    let mut wrote_scenario = false;

    for &scenario in &report.config.scenarios {
        let baseline = report
            .summaries
            .iter()
            .find(|summary| summary.scenario == scenario && summary.label == "baseline");
        let candidate = report
            .summaries
            .iter()
            .find(|summary| summary.scenario == scenario && summary.label == "candidate");
        let (Some(baseline), Some(candidate)) = (baseline, candidate) else { continue };

        if wrote_scenario {
            output.push('\n');
        }
        wrote_scenario = true;

        let metrics = [
            (
                "Wall p50 (ms)",
                baseline.scenario_wall_ms.count,
                baseline.scenario_wall_ms.p50,
                candidate.scenario_wall_ms.count,
                candidate.scenario_wall_ms.p50,
            ),
            (
                "Wall p95 (ms)",
                baseline.scenario_wall_ms.count,
                baseline.scenario_wall_ms.p95,
                candidate.scenario_wall_ms.count,
                candidate.scenario_wall_ms.p95,
            ),
            (
                "Peak RSS max (MiB)",
                baseline.peak_rss_mib.count,
                baseline.peak_rss_mib.max,
                candidate.peak_rss_mib.count,
                candidate.peak_rss_mib.max,
            ),
        ];
        for (index, (metric, baseline_count, baseline_value, candidate_count, candidate_value)) in
            metrics.into_iter().enumerate()
        {
            let scenario = if index == 0 { scenario.name() } else { "" };
            let baseline = display_value(baseline_count, baseline_value);
            let candidate = display_value(candidate_count, candidate_value);
            let change =
                display_change(baseline_count, baseline_value, candidate_count, candidate_value);
            let _ = writeln!(
                output,
                "{scenario:<22}  {metric:<22}  {baseline:>11}  {candidate:>13}  {change:>10}",
            );
        }
    }

    output
}

fn display_value(count: usize, value: f64) -> String {
    if count == 0 { "n/a".into() } else { format!("{value:.2}") }
}

fn display_change(
    baseline_count: usize,
    baseline: f64,
    candidate_count: usize,
    candidate: f64,
) -> String {
    if baseline_count == 0 || candidate_count == 0 || baseline == 0.0 {
        "n/a".into()
    } else {
        format!("{:+.2}%", (candidate / baseline - 1.0) * 100.0)
    }
}

fn markdown(report: &SummaryReport) -> String {
    let mut output = String::from(
        "# Solar LSP benchmark\n\n| Binary | Scenario | Runs | Wall p50 | Wall p95 | Analysis p95 | CPU p50 | Peak RSS max |\n|---|---|---:|---:|---:|---:|---:|---:|\n",
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
    use snapbox::{assert_data_eq, str};
    use std::path::PathBuf;

    #[test]
    fn terminal_report_makes_metrics_and_comparison_explicit() {
        let report = SummaryReport {
            schema_version: SCHEMA_VERSION,
            config: HarnessConfig {
                repeat: 10,
                timeout_ms: 10_000,
                scenarios: vec![Scenario::SlowTyping],
                fixture_file_count: 184,
            },
            environment: Environment::current(),
            binaries: Vec::new(),
            summaries: vec![
                scenario_summary("baseline", 100.0, 120.0, 50.0),
                scenario_summary("candidate", 90.0, 126.0, 40.0),
            ],
        };

        assert_data_eq!(
            terminal(&report),
            str![[r#"
Solar LSP benchmark results
Lower is better; change compares candidate with baseline.

Scenario                Metric                     Baseline      Candidate      Change
----------------------  ----------------------  -----------  -------------  ----------
slow-typing             Wall p50 (ms)                100.00          90.00     -10.00%
                        Wall p95 (ms)                120.00         126.00      +5.00%
                        Peak RSS max (MiB)            50.00          40.00     -20.00%

"#]],
        );
    }

    fn scenario_summary(
        label: &'static str,
        wall_p50: f64,
        wall_p95: f64,
        peak_rss_max: f64,
    ) -> ScenarioSummary {
        let empty = Stats::new(&[]);
        ScenarioSummary {
            label,
            scenario: Scenario::SlowTyping,
            successful_runs: 10,
            failed_runs: 0,
            scenario_wall_ms: Stats {
                count: 10,
                mean: wall_p50,
                p50: wall_p50,
                p95: wall_p95,
                max: wall_p95,
            },
            analysis_latency_ms: empty,
            request_latency_ms: empty,
            process_cpu_ms: empty,
            peak_rss_mib: Stats {
                count: 10,
                mean: peak_rss_max,
                p50: peak_rss_max,
                p95: peak_rss_max,
                max: peak_rss_max,
            },
            diagnostic_publications: empty,
        }
    }

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
                scenario: Scenario::SlowTyping,
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
                scenario: Scenario::SlowTyping,
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
                scenarios: vec![Scenario::SlowTyping],
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
