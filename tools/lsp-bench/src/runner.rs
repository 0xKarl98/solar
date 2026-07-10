use crate::{
    fixture::{Fixture, SOLIDITY_FILE_COUNT, STARTUP_MARKER},
    process::{LspProcess, Observations},
    report::{
        BinaryMetadata, HarnessConfig, RunSample, RunStatus, SummaryReport, summarize,
        write_reports,
    },
    scenario::{RunSpec, Scenario, Selection, comparison_plan},
};
use anyhow::{Context, Result, bail};
use lsp_types::{Position, Url};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

const SLOW_TYPING_TEXT: &str = "_slow_typing_exercises_completed";
const FAST_TYPING_TEXT: &str = "_fast_typing_produces_a_burst_while_earlier_analyses_are_running";
const REQUEST_TYPING_TEXT: &str = "_requests_during_edit";
const NAVIGATION_MODULES: [usize; 9] = [0, 20, 40, 60, 80, 100, 120, 140, 160];
const CROSS_FILE_MODULES: [usize; 4] = [20, 60, 100, 140];

pub(crate) struct CompareOptions {
    pub(crate) baseline: PathBuf,
    pub(crate) candidate: PathBuf,
    pub(crate) selection: Selection,
    pub(crate) repeat: usize,
    pub(crate) timeout: Duration,
    pub(crate) output: PathBuf,
}

pub(crate) struct CompareOutcome {
    pub(crate) summary: SummaryReport,
    pub(crate) failed_runs: usize,
}

pub(crate) fn compare(options: CompareOptions) -> Result<CompareOutcome> {
    if options.repeat == 0 {
        bail!("`--repeat` must be greater than zero")
    }
    let baseline = canonical_binary(&options.baseline)?;
    let candidate = canonical_binary(&options.candidate)?;
    let scenarios = options.selection.scenarios();
    let binaries =
        vec![binary_metadata("baseline", &baseline)?, binary_metadata("candidate", &candidate)?];
    validate_build_settings(&binaries)?;
    let plan = comparison_plan(&baseline, &candidate, &scenarios, options.repeat);
    let mut samples = Vec::with_capacity(plan.len());

    for (index, spec) in plan.iter().enumerate() {
        eprintln!(
            "[{}/{}] {} {} repetition {}",
            index + 1,
            plan.len(),
            spec.label,
            spec.scenario.name(),
            spec.repetition + 1,
        );
        samples.push(run_once(spec, options.timeout));
    }

    let config = HarnessConfig {
        repeat: options.repeat,
        timeout_ms: options.timeout.as_millis().try_into().unwrap_or(u64::MAX),
        scenarios,
        fixture_file_count: SOLIDITY_FILE_COUNT,
    };
    let summary = summarize(config, binaries, &samples);
    write_reports(&options.output, &summary, &samples)?;
    let failed_runs = samples.iter().filter(|sample| !sample.succeeded()).count();
    Ok(CompareOutcome { summary, failed_runs })
}

fn canonical_binary(path: &Path) -> Result<PathBuf> {
    path.canonicalize().with_context(|| format!("binary `{}` does not exist", path.display()))
}

fn binary_metadata(label: &'static str, path: &Path) -> Result<BinaryMetadata> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to inspect `{}`", path.display()))?;
    if !output.status.success() {
        bail!("`{} --version` failed", path.display())
    }
    Ok(BinaryMetadata {
        label,
        path: path.to_path_buf(),
        version: String::from_utf8_lossy(&output.stdout).trim().to_string(),
    })
}

#[derive(Debug, Eq, PartialEq)]
struct BuildSettings {
    profile: String,
    features: Vec<String>,
}

fn validate_build_settings(binaries: &[BinaryMetadata]) -> Result<()> {
    let [baseline, candidate] = binaries else { bail!("comparison requires exactly two binaries") };
    let baseline_settings = build_settings(&baseline.version)?;
    let candidate_settings = build_settings(&candidate.version)?;
    if baseline_settings != candidate_settings {
        bail!(
            "baseline and candidate build settings differ: baseline {baseline_settings:?}, candidate {candidate_settings:?}"
        )
    }
    Ok(())
}

fn build_settings(version: &str) -> Result<BuildSettings> {
    let field = |name: &str| {
        version
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
            .with_context(|| format!("`solar --version` is missing `{name}`"))
    };
    let mut features = field("Build Features:")?
        .split(',')
        .filter(|feature| !feature.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    features.sort();
    Ok(BuildSettings { profile: field("Build Profile:")?.to_string(), features })
}

fn run_once(spec: &RunSpec, timeout: Duration) -> RunSample {
    let fixture = match Fixture::create() {
        Ok(fixture) => fixture,
        Err(error) => return failed_sample(spec, error, Observations::default()),
    };
    let process_started = Instant::now();
    let process = match LspProcess::spawn(&spec.binary, fixture.root(), timeout) {
        Ok(process) => process,
        Err(error) => return failed_sample(spec, error, Observations::default()),
    };
    let mut session = BenchmarkSession::new(process, fixture);
    let initialized = session.initialize();
    if let Err(error) = initialized {
        let observations = session.process.observations().clone();
        let _ = session.process.finish(false);
        return failed_sample(spec, error, observations);
    }

    let result = if spec.scenario == Scenario::Startup {
        session.run_startup(process_started)
    } else {
        session.prepare().and_then(|()| {
            session.process.clear_observations();
            session.run(spec.scenario)
        })
    };
    let observations = session.process.observations().clone();
    let graceful = result.is_ok();
    let process = session.process.finish(graceful);

    match (result, process) {
        (Ok(outcome), Ok(process)) if process.exit_code == Some(0) => RunSample {
            label: spec.label,
            binary: spec.binary.clone(),
            scenario: spec.scenario,
            repetition: spec.repetition,
            status: RunStatus::Ok,
            scenario_wall_ms: Some(outcome.wall_ms),
            analysis_latencies_ms: outcome.analysis_latencies_ms,
            process: Some(process),
            observations,
            error: None,
        },
        (result, process) => {
            let error = match (result.err(), process.as_ref().err()) {
                (Some(run), Some(stop)) => {
                    format!("{run:#}; additionally failed to stop LSP: {stop:#}")
                }
                (Some(run), None) => format!("{run:#}"),
                (None, Some(stop)) => format!("{stop:#}"),
                (None, None) => format!(
                    "LSP exited unsuccessfully with {:?}",
                    process.as_ref().ok().and_then(|metrics| metrics.exit_code)
                ),
            };
            RunSample {
                label: spec.label,
                binary: spec.binary.clone(),
                scenario: spec.scenario,
                repetition: spec.repetition,
                status: RunStatus::Failed,
                scenario_wall_ms: None,
                analysis_latencies_ms: Vec::new(),
                process: process.ok(),
                observations,
                error: Some(error),
            }
        }
    }
}

fn failed_sample(spec: &RunSpec, error: anyhow::Error, observations: Observations) -> RunSample {
    RunSample {
        label: spec.label,
        binary: spec.binary.clone(),
        scenario: spec.scenario,
        repetition: spec.repetition,
        status: RunStatus::Failed,
        scenario_wall_ms: None,
        analysis_latencies_ms: Vec::new(),
        process: None,
        observations,
        error: Some(format!("{error:#}")),
    }
}

struct ScenarioOutcome {
    wall_ms: f64,
    analysis_latencies_ms: Vec<f64>,
}

struct BenchmarkSession {
    process: LspProcess,
    fixture: Fixture,
    documents: BTreeMap<PathBuf, Document>,
}

impl BenchmarkSession {
    fn new(process: LspProcess, fixture: Fixture) -> Self {
        let mut process = process;
        if let Ok(uri) = file_uri(&fixture.treasury_path()) {
            process.observe_diagnostics_for(uri.to_string());
        }
        Self { process, fixture, documents: BTreeMap::new() }
    }

    fn initialize(&mut self) -> Result<()> {
        let root_uri = file_uri(self.fixture.root())?;
        self.process.setup_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "solar-lsp-bench", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": root_uri,
                "capabilities": {
                    "workspace": {
                        "workspaceFolders": true,
                        "didChangeWatchedFiles": {"dynamicRegistration": true}
                    },
                    "textDocument": {
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": true}
                    }
                },
                "initializationOptions": {"flychecks": []},
                "workspaceFolders": [{"uri": root_uri, "name": "solar-lsp-bench"}]
            }),
        )?;
        self.process.notify("initialized", json!({}))
    }

    fn prepare(&mut self) -> Result<()> {
        let path = self.fixture.treasury_path();
        self.open_document(&path)?;
        self.process.wait_for_diagnostics(STARTUP_MARKER)?;
        Ok(())
    }

    fn run_startup(&mut self, started_at: Instant) -> Result<ScenarioOutcome> {
        let path = self.fixture.treasury_path();
        self.open_document(&path)?;
        let latency = self.process.wait_for_diagnostics(STARTUP_MARKER)?;
        Ok(ScenarioOutcome {
            wall_ms: duration_ms(started_at.elapsed()),
            analysis_latencies_ms: vec![latency],
        })
    }

    fn run(&mut self, scenario: Scenario) -> Result<ScenarioOutcome> {
        match scenario {
            Scenario::Startup => unreachable!(),
            Scenario::SlowTyping => self.typing(SLOW_TYPING_TEXT, Duration::from_millis(110)),
            Scenario::FastTyping => self.typing(FAST_TYPING_TEXT, Duration::from_millis(2)),
            Scenario::FileNavigation => self.file_navigation(),
            Scenario::CrossFileEdits => self.cross_file_edits(),
            Scenario::RequestsDuringEdit => self.requests_during_edit(),
            Scenario::WatchedFiles => self.watched_files(),
        }
    }

    fn typing(&mut self, text: &str, interval: Duration) -> Result<ScenarioOutcome> {
        let started_at = Instant::now();
        let path = self.fixture.treasury_path();
        let mut marker = STARTUP_MARKER.to_string();
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            self.append_to_marker(&path, &mut marker, character)?;
            if characters.peek().is_some() {
                thread::sleep(interval);
            }
        }
        let latency = self.process.wait_for_diagnostics(&marker)?;
        Ok(ScenarioOutcome {
            wall_ms: duration_ms(started_at.elapsed()),
            analysis_latencies_ms: vec![latency],
        })
    }

    fn file_navigation(&mut self) -> Result<ScenarioOutcome> {
        let started_at = Instant::now();
        let mut latencies = Vec::new();
        let mut paths = NAVIGATION_MODULES.map(|module| self.fixture.module_path(module)).to_vec();
        paths.extend([
            self.fixture.root().join("src/Owned.sol"),
            self.fixture.root().join("src/lib/PercentMath.sol"),
            self.fixture.treasury_path(),
        ]);

        for path in paths {
            if self.documents.contains_key(&path) {
                let uri = file_uri(&path)?;
                self.process.request(
                    "textDocument/documentSymbol",
                    json!({"textDocument": {"uri": uri}}),
                )?;
            } else {
                self.open_document(&path)?;
                latencies.push(self.process.wait_for_diagnostics(STARTUP_MARKER)?);
            }
        }
        Ok(ScenarioOutcome {
            wall_ms: duration_ms(started_at.elapsed()),
            analysis_latencies_ms: latencies,
        })
    }

    fn cross_file_edits(&mut self) -> Result<ScenarioOutcome> {
        let started_at = Instant::now();
        let mut latencies = Vec::new();
        for module in CROSS_FILE_MODULES {
            let path = self.fixture.module_path(module);
            self.open_document(&path)?;
            latencies.push(self.process.wait_for_diagnostics(STARTUP_MARKER)?);
            let mut marker = format!("cross_{module:03}");
            let mut characters = "_edited_during_profiling".chars().peekable();
            while let Some(character) = characters.next() {
                self.append_to_marker(&path, &mut marker, character)?;
                if characters.peek().is_some() {
                    thread::sleep(Duration::from_millis(4));
                }
            }
            self.remove_prefix_before_marker(&path, &marker, "// ")?;
            latencies.push(self.process.wait_for_diagnostics(&marker)?);
        }
        for path in CROSS_FILE_MODULES.map(|module| self.fixture.module_path(module)) {
            let uri = file_uri(&path)?;
            self.process.notify("textDocument/didSave", json!({"textDocument": {"uri": uri}}))?;
        }
        Ok(ScenarioOutcome {
            wall_ms: duration_ms(started_at.elapsed()),
            analysis_latencies_ms: latencies,
        })
    }

    fn requests_during_edit(&mut self) -> Result<ScenarioOutcome> {
        let started_at = Instant::now();
        let path = self.fixture.treasury_path();
        let uri = file_uri(&path)?;
        let mut marker = STARTUP_MARKER.to_string();
        let character_count = REQUEST_TYPING_TEXT.chars().count();
        for (index, character) in REQUEST_TYPING_TEXT.chars().enumerate() {
            let position = self.append_to_marker(&path, &mut marker, character)?;
            match index {
                2 => {
                    self.process.request(
                        "textDocument/completion",
                        json!({"textDocument": {"uri": uri}, "position": position}),
                    )?;
                }
                5 => {
                    self.process.request(
                        "textDocument/definition",
                        json!({"textDocument": {"uri": uri}, "position": position}),
                    )?;
                }
                8 => {
                    self.process.request(
                        "textDocument/references",
                        json!({
                            "textDocument": {"uri": uri},
                            "position": position,
                            "context": {"includeDeclaration": true}
                        }),
                    )?;
                }
                11 => {
                    let end = end_position(&self.documents[&path].text);
                    self.process.request(
                        "textDocument/inlayHint",
                        json!({
                            "textDocument": {"uri": uri},
                            "range": {"start": {"line": 0, "character": 0}, "end": end}
                        }),
                    )?;
                }
                _ => {}
            }
            if index + 1 < character_count {
                thread::sleep(Duration::from_millis(2));
            }
        }
        let latency = self.process.wait_for_diagnostics(&marker)?;
        Ok(ScenarioOutcome {
            wall_ms: duration_ms(started_at.elapsed()),
            analysis_latencies_ms: vec![latency],
        })
    }

    fn watched_files(&mut self) -> Result<ScenarioOutcome> {
        let started_at = Instant::now();
        let path = self.fixture.root().join("src/Watched.sol");
        let uri = file_uri(&path)?;
        let mut latencies = Vec::new();

        fs::write(&path, "pragma solidity ^0.8.0; contract Watched {}\n")?;
        self.watched_file_event(&uri, 1)?;
        latencies.push(self.process.wait_for_diagnostics(STARTUP_MARKER)?);

        fs::write(&path, "pragma solidity ^0.8.0; contract Watched { uint256 value; }\n")?;
        self.watched_file_event(&uri, 2)?;
        latencies.push(self.process.wait_for_diagnostics(STARTUP_MARKER)?);

        fs::remove_file(&path)?;
        self.watched_file_event(&uri, 3)?;
        latencies.push(self.process.wait_for_diagnostics(STARTUP_MARKER)?);

        Ok(ScenarioOutcome {
            wall_ms: duration_ms(started_at.elapsed()),
            analysis_latencies_ms: latencies,
        })
    }

    fn watched_file_event(&mut self, uri: &Url, typ: u8) -> Result<()> {
        self.process.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes": [{"uri": uri, "type": typ}]}),
        )
    }

    fn open_document(&mut self, path: &Path) -> Result<()> {
        if self.documents.contains_key(path) {
            return Ok(());
        }
        let text = fs::read_to_string(path)?;
        let uri = file_uri(path)?;
        self.process.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "solidity",
                    "version": 1,
                    "text": text
                }
            }),
        )?;
        self.documents.insert(path.to_path_buf(), Document { text, version: 1 });
        Ok(())
    }

    fn append_to_marker(
        &mut self,
        path: &Path,
        marker: &mut String,
        character: char,
    ) -> Result<Position> {
        let uri = file_uri(path)?;
        let (version, position) = {
            let document = self.documents.get_mut(path).context("document is not open")?;
            let offset = document
                .text
                .find(marker.as_str())
                .map(|offset| offset + marker.len())
                .with_context(|| {
                    format!("marker `{marker}` is missing from `{}`", path.display())
                })?;
            let position = position_at(&document.text, offset);
            document.text.insert(offset, character);
            document.version += 1;
            marker.push(character);
            (document.version, position)
        };
        self.process.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{
                    "range": {"start": position, "end": position},
                    "text": character.to_string()
                }]
            }),
        )?;
        Ok(position)
    }

    fn remove_prefix_before_marker(
        &mut self,
        path: &Path,
        marker: &str,
        prefix: &str,
    ) -> Result<()> {
        let uri = file_uri(path)?;
        let (version, start, end) = {
            let document = self.documents.get_mut(path).context("document is not open")?;
            let marker_offset = document.text.find(marker).with_context(|| {
                format!("marker `{marker}` is missing from `{}`", path.display())
            })?;
            let prefix_offset =
                marker_offset.checked_sub(prefix.len()).context("marker prefix is missing")?;
            if &document.text[prefix_offset..marker_offset] != prefix {
                bail!("expected `{prefix}` before marker `{marker}`")
            }
            let start = position_at(&document.text, prefix_offset);
            let end = position_at(&document.text, marker_offset);
            document.text.replace_range(prefix_offset..marker_offset, "");
            document.version += 1;
            (document.version, start, end)
        };
        self.process.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"range": {"start": start, "end": end}, "text": ""}]
            }),
        )
    }
}

struct Document {
    text: String,
    version: i32,
}

fn file_uri(path: &Path) -> Result<Url> {
    Url::from_file_path(path)
        .map_err(|()| anyhow::anyhow!("invalid file path `{}`", path.display()))
}

fn position_at(text: &str, offset: usize) -> Position {
    let prefix = &text[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
    let character = prefix[line_start..].encode_utf16().count() as u32;
    Position { line, character }
}

fn end_position(text: &str) -> Position {
    position_at(text, text.len())
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_use_utf16_code_units() {
        assert_eq!(position_at("a😀\nb", "a😀\nb".len()), Position { line: 1, character: 1 });
        assert_eq!(position_at("a😀", "a😀".len()), Position { line: 0, character: 3 });
    }

    #[test]
    fn build_settings_ignore_feature_order() {
        let first = "Build Features: tracing,mimalloc\nBuild Profile: profiling";
        let second = "Build Features: mimalloc,tracing\nBuild Profile: profiling";

        assert_eq!(build_settings(first).unwrap(), build_settings(second).unwrap());
    }
}
