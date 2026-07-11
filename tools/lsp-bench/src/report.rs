use crate::{
    fixture::FixtureMetadata,
    process::{Observations, ProcessMetrics},
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

pub(crate) const SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BinaryMetadata {
    pub(crate) label: &'static str,
    pub(crate) path: PathBuf,
    pub(crate) version: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ForgeMetadata {
    pub(crate) path: PathBuf,
    pub(crate) version: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct HarnessConfig {
    pub(crate) repeat: usize,
    pub(crate) timeout_ms: u64,
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
pub(crate) struct LatencyMeasurement {
    pub(crate) name: &'static str,
    pub(crate) elapsed_ms: f64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AnalysisActivity {
    pub(crate) phase: &'static str,
    pub(crate) solar_analysis_triggers: usize,
    pub(crate) solar_diagnostic_publications: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SessionOutcome {
    pub(crate) latencies: Vec<LatencyMeasurement>,
    pub(crate) analysis_activity: Vec<AnalysisActivity>,
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
    pub(crate) repetition: usize,
    pub(crate) status: RunStatus,
    pub(crate) outcome: Option<SessionOutcome>,
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
pub(crate) struct NamedStats {
    pub(crate) name: String,
    pub(crate) stats: Stats,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AnalysisSummary {
    pub(crate) phase: String,
    pub(crate) solar_analysis_triggers: Stats,
    pub(crate) solar_diagnostic_publications: Stats,
    pub(crate) unpublished_analysis_proxy: Stats,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BinarySummary {
    pub(crate) label: &'static str,
    pub(crate) successful_runs: usize,
    pub(crate) failed_runs: usize,
    pub(crate) latencies: Vec<NamedStats>,
    pub(crate) requests: Vec<NamedStats>,
    pub(crate) analysis_activity: Vec<AnalysisSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SummaryReport {
    pub(crate) schema_version: u32,
    pub(crate) config: HarnessConfig,
    pub(crate) environment: Environment,
    pub(crate) fixture: FixtureMetadata,
    pub(crate) forge: ForgeMetadata,
    pub(crate) binaries: Vec<BinaryMetadata>,
    pub(crate) summaries: Vec<BinarySummary>,
}

#[derive(Serialize)]
struct SamplesReport<'a> {
    schema_version: u32,
    samples: &'a [RunSample],
}

pub(crate) fn summarize(
    config: HarnessConfig,
    binaries: Vec<BinaryMetadata>,
    fixture: FixtureMetadata,
    forge: ForgeMetadata,
    samples: &[RunSample],
) -> Result<SummaryReport> {
    let mut summaries = Vec::with_capacity(binaries.len());
    for binary in &binaries {
        let runs = samples.iter().filter(|sample| sample.label == binary.label).collect::<Vec<_>>();
        let successful =
            runs.iter().copied().filter(|sample| sample.succeeded()).collect::<Vec<_>>();
        let mut latencies = BTreeMap::<&str, Vec<f64>>::new();
        let mut requests = BTreeMap::<&str, Vec<f64>>::new();
        let mut activity = BTreeMap::<&str, ActivityValues>::new();

        for sample in &successful {
            let outcome =
                sample.outcome.as_ref().context("successful run has no session outcome")?;
            for measurement in &outcome.latencies {
                latencies.entry(measurement.name).or_default().push(measurement.elapsed_ms);
            }
            for request in &sample.observations.requests {
                requests.entry(&request.method).or_default().push(request.elapsed_ms);
            }
            for measurement in &outcome.analysis_activity {
                let values = activity.entry(measurement.phase).or_default();
                values.triggers.push(measurement.solar_analysis_triggers as f64);
                values.publications.push(measurement.solar_diagnostic_publications as f64);
                values.proxies.push(unpublished_analysis_proxy(measurement)? as f64);
            }
        }

        summaries.push(BinarySummary {
            label: binary.label,
            successful_runs: successful.len(),
            failed_runs: runs.len() - successful.len(),
            latencies: named_stats(latencies),
            requests: named_stats(requests),
            analysis_activity: activity
                .into_iter()
                .map(|(phase, values)| AnalysisSummary {
                    phase: phase.into(),
                    solar_analysis_triggers: Stats::new(&values.triggers),
                    solar_diagnostic_publications: Stats::new(&values.publications),
                    unpublished_analysis_proxy: Stats::new(&values.proxies),
                })
                .collect(),
        });
    }

    Ok(SummaryReport {
        schema_version: SCHEMA_VERSION,
        config,
        environment: Environment::current(),
        fixture,
        forge,
        binaries,
        summaries,
    })
}

#[derive(Default)]
struct ActivityValues {
    triggers: Vec<f64>,
    publications: Vec<f64>,
    proxies: Vec<f64>,
}

fn unpublished_analysis_proxy(activity: &AnalysisActivity) -> Result<usize> {
    activity
        .solar_analysis_triggers
        .checked_sub(activity.solar_diagnostic_publications)
        .with_context(|| {
            format!(
                "phase `{}` published {} Solar diagnostics for {} analysis triggers",
                activity.phase,
                activity.solar_diagnostic_publications,
                activity.solar_analysis_triggers,
            )
        })
}

fn named_stats(values: BTreeMap<&str, Vec<f64>>) -> Vec<NamedStats> {
    values
        .into_iter()
        .map(|(name, values)| NamedStats { name: name.into(), stats: Stats::new(&values) })
        .collect()
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
         Latency is in milliseconds; change compares candidate p50 with baseline p50.\n\n",
    );
    let (baseline, candidate) = binary_summaries(report);

    output.push_str("Edit latency (lower is better)\n");
    write_latency_table(&mut output, baseline, candidate, |name| {
        name.starts_with("edit_settle_ms")
    });

    output.push_str("\nRequest latency (lower is better)\n");
    write_named_table(
        &mut output,
        baseline.map(|it| &it.requests),
        candidate.map(|it| &it.requests),
    );

    output.push_str("\nForge save lifecycle (lower is better; includes external Forge)\n");
    write_latency_table(&mut output, baseline, candidate, |name| name == "forge_flycheck_ready_ms");

    output.push_str(
        "\nSolar analysis activity (behavioral metrics; lower is not inherently better)\n",
    );
    write_activity_table(&mut output, baseline, candidate);

    output
        .push_str("\nRuns\nBinary         Successful  Failed\n-------------  ----------  ------\n");
    for summary in &report.summaries {
        let _ = writeln!(
            output,
            "{:<13}  {:>10}  {:>6}",
            summary.label, summary.successful_runs, summary.failed_runs
        );
    }
    output
}

fn binary_summaries(report: &SummaryReport) -> (Option<&BinarySummary>, Option<&BinarySummary>) {
    (
        report.summaries.iter().find(|summary| summary.label == "baseline"),
        report.summaries.iter().find(|summary| summary.label == "candidate"),
    )
}

fn write_latency_table(
    output: &mut String,
    baseline: Option<&BinarySummary>,
    candidate: Option<&BinarySummary>,
    include: impl Fn(&str) -> bool,
) {
    let baseline = baseline.map(|summary| &summary.latencies);
    let candidate = candidate.map(|summary| &summary.latencies);
    write_filtered_named_table(output, baseline, candidate, include);
}

fn write_named_table(
    output: &mut String,
    baseline: Option<&Vec<NamedStats>>,
    candidate: Option<&Vec<NamedStats>>,
) {
    write_filtered_named_table(output, baseline, candidate, |_| true);
}

fn write_filtered_named_table(
    output: &mut String,
    baseline: Option<&Vec<NamedStats>>,
    candidate: Option<&Vec<NamedStats>>,
    include: impl Fn(&str) -> bool,
) {
    output.push_str(
        "Metric                                  Baseline p50/max  Candidate p50/max  Change\n\
         --------------------------------------  ----------------  -----------------  -------\n",
    );
    let names = named_keys(baseline, candidate).into_iter().filter(|name| include(name));
    for name in names {
        let baseline_stats = find_named(baseline, name);
        let candidate_stats = find_named(candidate, name);
        let baseline_value = display_stats(baseline_stats);
        let candidate_value = display_stats(candidate_stats);
        let change = display_change(baseline_stats, candidate_stats);
        let _ = writeln!(
            output,
            "{name:<38}  {baseline_value:>16}  {candidate_value:>17}  {change:>7}"
        );
    }
}

fn write_activity_table(
    output: &mut String,
    baseline: Option<&BinarySummary>,
    candidate: Option<&BinarySummary>,
) {
    output.push_str(
        "Phase                       Metric                         Baseline p50/max  Candidate p50/max\n\
         --------------------------  -----------------------------  ----------------  -----------------\n",
    );
    let phases = activity_phases(baseline, candidate);
    for phase in phases {
        let baseline = find_activity(baseline, phase);
        let candidate = find_activity(candidate, phase);
        let metrics = [
            (
                "solar_analysis_triggers",
                baseline.map(|it| &it.solar_analysis_triggers),
                candidate.map(|it| &it.solar_analysis_triggers),
            ),
            (
                "solar_diagnostic_publications",
                baseline.map(|it| &it.solar_diagnostic_publications),
                candidate.map(|it| &it.solar_diagnostic_publications),
            ),
            (
                "unpublished_analysis_proxy",
                baseline.map(|it| &it.unpublished_analysis_proxy),
                candidate.map(|it| &it.unpublished_analysis_proxy),
            ),
        ];
        for (index, (metric, baseline, candidate)) in metrics.into_iter().enumerate() {
            let phase = if index == 0 { phase } else { "" };
            let baseline = display_stats(baseline);
            let candidate = display_stats(candidate);
            let _ = writeln!(output, "{phase:<26}  {metric:<29}  {baseline:>16}  {candidate:>17}");
        }
    }
}

fn named_keys<'a>(
    baseline: Option<&'a Vec<NamedStats>>,
    candidate: Option<&'a Vec<NamedStats>>,
) -> BTreeSet<&'a str> {
    baseline.into_iter().chain(candidate).flatten().map(|metric| metric.name.as_str()).collect()
}

fn activity_phases<'a>(
    baseline: Option<&'a BinarySummary>,
    candidate: Option<&'a BinarySummary>,
) -> BTreeSet<&'a str> {
    baseline
        .into_iter()
        .chain(candidate)
        .flat_map(|summary| &summary.analysis_activity)
        .map(|summary| summary.phase.as_str())
        .collect()
}

fn find_named<'a>(metrics: Option<&'a Vec<NamedStats>>, name: &str) -> Option<&'a Stats> {
    metrics?.iter().find(|metric| metric.name == name).map(|metric| &metric.stats)
}

fn find_activity<'a>(
    summary: Option<&'a BinarySummary>,
    phase: &str,
) -> Option<&'a AnalysisSummary> {
    summary?.analysis_activity.iter().find(|activity| activity.phase == phase)
}

fn display_stats(stats: Option<&Stats>) -> String {
    match stats.filter(|stats| stats.count != 0) {
        Some(stats) => format!("{:.2}/{:.2}", stats.p50, stats.max),
        None => "n/a".into(),
    }
}

fn display_change(baseline: Option<&Stats>, candidate: Option<&Stats>) -> String {
    match (
        baseline.filter(|stats| stats.count != 0 && stats.p50 != 0.0),
        candidate.filter(|stats| stats.count != 0),
    ) {
        (Some(baseline), Some(candidate)) => {
            format!("{:+.2}%", (candidate.p50 / baseline.p50 - 1.0) * 100.0)
        }
        _ => "n/a".into(),
    }
}

fn markdown(report: &SummaryReport) -> String {
    let mut output = format!(
        "# Solar LSP benchmark\n\nSolady `{}`: {} Solidity files, {} lines, {} bytes. Forge: `{}`.\n\n",
        report.fixture.revision,
        report.fixture.source_file_count,
        report.fixture.source_line_count,
        report.fixture.source_byte_count,
        report.forge.version,
    );
    let (baseline, candidate) = binary_summaries(report);
    markdown_named_section(
        &mut output,
        "Edit latency",
        baseline.map(|it| &it.latencies),
        candidate.map(|it| &it.latencies),
        |name| name.starts_with("edit_settle_ms"),
    );
    markdown_named_section(
        &mut output,
        "Request latency",
        baseline.map(|it| &it.requests),
        candidate.map(|it| &it.requests),
        |_| true,
    );
    markdown_named_section(
        &mut output,
        "Forge save lifecycle",
        baseline.map(|it| &it.latencies),
        candidate.map(|it| &it.latencies),
        |name| name == "forge_flycheck_ready_ms",
    );

    output.push_str("## Solar analysis activity\n\n| Phase | Metric | Baseline p50/max | Candidate p50/max |\n|---|---|---:|---:|\n");
    for phase in activity_phases(baseline, candidate) {
        let baseline = find_activity(baseline, phase);
        let candidate = find_activity(candidate, phase);
        for (metric, baseline, candidate) in [
            (
                "solar_analysis_triggers",
                baseline.map(|it| &it.solar_analysis_triggers),
                candidate.map(|it| &it.solar_analysis_triggers),
            ),
            (
                "solar_diagnostic_publications",
                baseline.map(|it| &it.solar_diagnostic_publications),
                candidate.map(|it| &it.solar_diagnostic_publications),
            ),
            (
                "unpublished_analysis_proxy",
                baseline.map(|it| &it.unpublished_analysis_proxy),
                candidate.map(|it| &it.unpublished_analysis_proxy),
            ),
        ] {
            let _ = writeln!(
                output,
                "| {phase} | {metric} | {} | {} |",
                display_stats(baseline),
                display_stats(candidate)
            );
        }
    }

    output.push_str("\n## Runs\n\n| Binary | Successful | Failed |\n|---|---:|---:|\n");
    for summary in &report.summaries {
        let _ = writeln!(
            output,
            "| {} | {} | {} |",
            summary.label, summary.successful_runs, summary.failed_runs
        );
    }
    output
}

fn markdown_named_section(
    output: &mut String,
    title: &str,
    baseline: Option<&Vec<NamedStats>>,
    candidate: Option<&Vec<NamedStats>>,
    include: impl Fn(&str) -> bool,
) {
    let _ = writeln!(
        output,
        "## {title}\n\n| Metric | Baseline p50/max | Candidate p50/max | Change |\n|---|---:|---:|---:|"
    );
    for name in named_keys(baseline, candidate).into_iter().filter(|name| include(name)) {
        let baseline = find_named(baseline, name);
        let candidate = find_named(candidate, name);
        let _ = writeln!(
            output,
            "| {name} | {} | {} | {} |",
            display_stats(baseline),
            display_stats(candidate),
            display_change(baseline, candidate)
        );
    }
    output.push('\n');
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
    use snapbox::{assert_data_eq, str};

    #[test]
    fn summary_uses_nearest_rank_percentiles() {
        let stats = Stats::new(&[1.0, 2.0, 3.0, 4.0, 100.0]);

        assert_eq!(stats.count, 5);
        assert_eq!(stats.p50, 3.0);
        assert_eq!(stats.p95, 100.0);
        assert_eq!(stats.max, 100.0);
    }

    #[test]
    fn summary_groups_metrics_and_excludes_failed_runs() {
        let samples = [
            sample(
                RunStatus::Ok,
                vec![LatencyMeasurement { name: "edit_settle_ms{slow}", elapsed_ms: 10.0 }],
                vec![activity("slow", 8, 1)],
            ),
            sample(RunStatus::Failed, Vec::new(), Vec::new()),
        ];

        let report = test_summary(&samples).unwrap();
        let summary = &report.summaries[0];
        assert_eq!(summary.successful_runs, 1);
        assert_eq!(summary.failed_runs, 1);
        assert_eq!(summary.latencies[0].name, "edit_settle_ms{slow}");
        assert_eq!(summary.latencies[0].stats.p50, 10.0);
        assert_eq!(summary.analysis_activity[0].unpublished_analysis_proxy.p50, 7.0);
    }

    #[test]
    fn proxy_is_checked_per_run_before_aggregation() {
        let samples = [
            sample(RunStatus::Ok, Vec::new(), vec![activity("slow", 10, 9)]),
            sample(RunStatus::Ok, Vec::new(), vec![activity("slow", 2, 0)]),
        ];

        let report = test_summary(&samples).unwrap();
        let proxy = report.summaries[0].analysis_activity[0].unpublished_analysis_proxy;
        assert_eq!(proxy.p50, 1.0);
        assert_eq!(proxy.max, 2.0);

        let invalid = [sample(RunStatus::Ok, Vec::new(), vec![activity("slow", 1, 2)])];
        assert!(test_summary(&invalid).is_err());
    }

    #[test]
    fn missing_values_are_rendered_as_na() {
        assert_eq!(display_stats(None), "n/a");
        assert_eq!(display_stats(Some(&Stats::new(&[]))), "n/a");
        assert_eq!(display_change(None, None), "n/a");
    }

    #[test]
    fn terminal_separates_latency_lifecycle_and_activity_metrics() {
        let report = SummaryReport {
            schema_version: SCHEMA_VERSION,
            config: HarnessConfig { repeat: 1, timeout_ms: 1_000 },
            environment: Environment::current(),
            fixture: FixtureMetadata {
                revision: "test".into(),
                source_file_count: 114,
                source_line_count: 53_156,
                source_byte_count: 2_317_649,
            },
            forge: ForgeMetadata { path: "forge".into(), version: "forge test".into() },
            binaries: Vec::new(),
            summaries: vec![summary("baseline", 10.0), summary("candidate", 8.0)],
        };

        assert_data_eq!(
            terminal(&report),
            str![[r#"
Solar LSP benchmark results
Latency is in milliseconds; change compares candidate p50 with baseline p50.

Edit latency (lower is better)
Metric                                  Baseline p50/max  Candidate p50/max  Change
--------------------------------------  ----------------  -----------------  -------
edit_settle_ms{slow}                         10.00/10.00          8.00/8.00  -20.00%

Request latency (lower is better)
Metric                                  Baseline p50/max  Candidate p50/max  Change
--------------------------------------  ----------------  -----------------  -------
textDocument/completion                      10.00/10.00          8.00/8.00  -20.00%

Forge save lifecycle (lower is better; includes external Forge)
Metric                                  Baseline p50/max  Candidate p50/max  Change
--------------------------------------  ----------------  -----------------  -------
forge_flycheck_ready_ms                      10.00/10.00          8.00/8.00  -20.00%

Solar analysis activity (behavioral metrics; lower is not inherently better)
Phase                       Metric                         Baseline p50/max  Candidate p50/max
--------------------------  -----------------------------  ----------------  -----------------
slow                        solar_analysis_triggers               8.00/8.00          8.00/8.00
                            solar_diagnostic_publications         1.00/1.00          1.00/1.00
                            unpublished_analysis_proxy            7.00/7.00          7.00/7.00

Runs
Binary         Successful  Failed
-------------  ----------  ------
baseline                1       0
candidate               1       0

"#]],
        );
    }

    fn test_summary(samples: &[RunSample]) -> Result<SummaryReport> {
        summarize(
            HarnessConfig { repeat: samples.len(), timeout_ms: 1_000 },
            vec![BinaryMetadata {
                label: "baseline",
                path: "solar".into(),
                version: "test".into(),
            }],
            FixtureMetadata {
                revision: "test".into(),
                source_file_count: 114,
                source_line_count: 53_156,
                source_byte_count: 2_317_649,
            },
            ForgeMetadata { path: "forge".into(), version: "test".into() },
            samples,
        )
    }

    fn sample(
        status: RunStatus,
        latencies: Vec<LatencyMeasurement>,
        analysis_activity: Vec<AnalysisActivity>,
    ) -> RunSample {
        let outcome = matches!(status, RunStatus::Ok)
            .then_some(SessionOutcome { latencies, analysis_activity });
        RunSample {
            label: "baseline",
            binary: "solar".into(),
            repetition: 0,
            status,
            outcome,
            process: None,
            observations: Observations::default(),
            error: None,
        }
    }

    fn activity(
        phase: &'static str,
        solar_analysis_triggers: usize,
        solar_diagnostic_publications: usize,
    ) -> AnalysisActivity {
        AnalysisActivity { phase, solar_analysis_triggers, solar_diagnostic_publications }
    }

    fn summary(label: &'static str, latency: f64) -> BinarySummary {
        let stats = Stats::new(&[latency]);
        BinarySummary {
            label,
            successful_runs: 1,
            failed_runs: 0,
            latencies: vec![
                NamedStats { name: "edit_settle_ms{slow}".into(), stats },
                NamedStats { name: "forge_flycheck_ready_ms".into(), stats },
            ],
            requests: vec![NamedStats { name: "textDocument/completion".into(), stats }],
            analysis_activity: vec![AnalysisSummary {
                phase: "slow".into(),
                solar_analysis_triggers: Stats::new(&[8.0]),
                solar_diagnostic_publications: Stats::new(&[1.0]),
                unpublished_analysis_proxy: Stats::new(&[7.0]),
            }],
        }
    }
}
