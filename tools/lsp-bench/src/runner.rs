use crate::{
    fixture::{
        CROSS_FILE_MARKER, FULL_MUL_DIV_CALL, FULL_MUL_DIV_DECLARATION, FULL_MUL_DIV_NAME, Fixture,
        Project, STARTUP_MARKER,
    },
    process::{FinishedProcess, LspProcess, Observations},
    report::{
        AnalysisActivity, BinaryMetadata, ForgeMetadata, HarnessConfig, LatencyMeasurement,
        RunSample, RunStatus, SessionOutcome, SummaryReport, summarize, write_reports,
    },
};
use anyhow::{Context, Result, bail};
use lsp_types::{CompletionResponse, GotoDefinitionResponse, InlayHint, Location, Position, Url};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const SLOW_TYPING_TEXT: &str = "slowedit";
const NORMAL_TYPING_TEXT: &str = "normal01";
const FAST_TYPING_TEXT: &str = "fastedit";
const CROSS_FILE_TYPING_TEXT: &str = "cross001";
const SLOW_TYPING_INTERVAL: Duration = Duration::from_millis(400);
const NORMAL_TYPING_INTERVAL: Duration = Duration::from_millis(240);
const FAST_TYPING_INTERVAL: Duration = Duration::from_millis(120);
const FORGE_DIAGNOSTIC_SOURCE: &str = "forge-lint";
const FORGE_DIAGNOSTIC_CODE: &str = "mixed-case-variable";

pub(crate) struct CompareOptions {
    pub(crate) baseline: PathBuf,
    pub(crate) candidate: PathBuf,
    pub(crate) project: PathBuf,
    pub(crate) forge: PathBuf,
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

    let baseline = canonical_executable(&options.baseline, "baseline")?;
    let candidate = canonical_executable(&options.candidate, "candidate")?;
    let project = Project::open(&options.project)?;
    let forge = canonical_executable(&options.forge, "Forge")?;
    let forge_metadata = validate_forge(&forge, project.root())?;
    let binaries =
        vec![binary_metadata("baseline", &baseline)?, binary_metadata("candidate", &candidate)?];
    validate_build_settings(&binaries)?;

    let plan = comparison_plan(&baseline, &candidate, options.repeat);
    let mut samples = Vec::with_capacity(plan.len());
    for (index, spec) in plan.iter().enumerate() {
        eprintln!(
            "[{}/{}] {} user-session repetition {}",
            index + 1,
            plan.len(),
            spec.label,
            spec.repetition + 1,
        );
        samples.push(run_once(spec, &project, &forge, options.timeout));
        if samples.len() % 2 == 0 {
            let pair_start = samples.len() - 2;
            validate_pair(&mut samples[pair_start..]);
        }
    }

    let config = HarnessConfig {
        repeat: options.repeat,
        timeout_ms: options.timeout.as_millis().try_into().unwrap_or(u64::MAX),
    };
    let summary =
        summarize(config, binaries, project.metadata().clone(), forge_metadata, &samples)?;
    write_reports(&options.output, &summary, &samples)?;
    let failed_runs = samples.iter().filter(|sample| !sample.succeeded()).count();
    Ok(CompareOutcome { summary, failed_runs })
}

fn canonical_executable(path: &Path, name: &str) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{name} executable `{}` does not exist", path.display()))?;
    if !path.is_file() {
        bail!("{name} executable `{}` is not a file", path.display())
    }
    Ok(path)
}

fn binary_metadata(label: &'static str, path: &Path) -> Result<BinaryMetadata> {
    Ok(BinaryMetadata { label, path: path.to_path_buf(), version: tool_version(path, None)? })
}

fn validate_forge(path: &Path, project: &Path) -> Result<ForgeMetadata> {
    let version = tool_version(path, Some(project))?;
    let status = Command::new(path)
        .args(["lint", "--help"])
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run `{} lint --help`", path.display()))?;
    if !status.success() {
        bail!("`{} lint --help` failed", path.display())
    }
    Ok(ForgeMetadata { path: path.to_path_buf(), version })
}

fn tool_version(path: &Path, cwd: Option<&Path>) -> Result<String> {
    let mut command = Command::new(path);
    command.arg("--version");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output =
        command.output().with_context(|| format!("failed to inspect `{}`", path.display()))?;
    if !output.status.success() {
        bail!("`{} --version` failed", path.display())
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let version = if stdout.trim().is_empty() { stderr.trim() } else { stdout.trim() };
    Ok(version.to_string())
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

#[derive(Clone)]
struct RunSpec {
    label: &'static str,
    binary: PathBuf,
    repetition: usize,
}

fn comparison_plan(baseline: &Path, candidate: &Path, repeat: usize) -> Vec<RunSpec> {
    let mut runs = Vec::with_capacity(repeat * 2);
    for repetition in 0..repeat {
        let pair = if repetition % 2 == 0 {
            [("baseline", baseline), ("candidate", candidate)]
        } else {
            [("candidate", candidate), ("baseline", baseline)]
        };
        runs.extend(pair.map(|(label, binary)| RunSpec {
            label,
            binary: binary.to_path_buf(),
            repetition,
        }));
    }
    runs
}

fn run_once(spec: &RunSpec, project: &Project, forge: &Path, timeout: Duration) -> RunSample {
    let fixture = match Fixture::copy_from(project) {
        Ok(fixture) => fixture,
        Err(error) => return failed_sample(spec, error, Observations::default()),
    };
    let process = match LspProcess::spawn(&spec.binary, fixture.root(), timeout) {
        Ok(process) => process,
        Err(error) => return failed_sample(spec, error, Observations::default()),
    };
    let mut session = match BenchmarkSession::new(process, fixture, forge.to_path_buf()) {
        Ok(session) => session,
        Err(error) => return failed_sample(spec, error, Observations::default()),
    };

    let result = session.initialize().and_then(|()| session.prepare()).and_then(|()| {
        session.reset_measurements();
        session.user_session()
    });
    let fallback_observations = session.process.observations().clone();
    let graceful = result.is_ok();
    let process = session.process.finish(graceful);

    match (result, process) {
        (Ok(outcome), Ok(FinishedProcess { metrics, observations }))
            if metrics.exit_code == Some(0) =>
        {
            RunSample {
                label: spec.label,
                binary: spec.binary.clone(),
                repetition: spec.repetition,
                status: RunStatus::Ok,
                outcome: Some(outcome),
                process: Some(metrics),
                observations,
                error: None,
            }
        }
        (result, process) => {
            let process_error = process.as_ref().err();
            let exit_error =
                process.as_ref().ok().filter(|finished| finished.metrics.exit_code != Some(0)).map(
                    |finished| {
                        format!("LSP exited unsuccessfully with {:?}", finished.metrics.exit_code)
                    },
                );
            let error = match (result.err(), process_error, exit_error) {
                (Some(run), Some(stop), _) => {
                    format!("{run:#}; additionally failed to stop LSP: {stop:#}")
                }
                (Some(run), None, _) => format!("{run:#}"),
                (None, Some(stop), _) => format!("{stop:#}"),
                (None, None, Some(exit)) => exit,
                (None, None, None) => "benchmark run failed".into(),
            };
            let (metrics, observations) = match process {
                Ok(finished) => (Some(finished.metrics), finished.observations),
                Err(_) => (None, fallback_observations),
            };
            RunSample {
                label: spec.label,
                binary: spec.binary.clone(),
                repetition: spec.repetition,
                status: RunStatus::Failed,
                outcome: None,
                process: metrics,
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
        repetition: spec.repetition,
        status: RunStatus::Failed,
        outcome: None,
        process: None,
        observations,
        error: Some(format!("{error:#}")),
    }
}

fn validate_pair(pair: &mut [RunSample]) {
    let [first, second] = pair else { return };
    if !first.succeeded() || !second.succeeded() {
        return;
    }
    let first_activity = trigger_counts(first);
    let second_activity = trigger_counts(second);
    if first_activity == second_activity {
        return;
    }

    let error = format!(
        "analysis trigger counts differ within repetition {}: {} {first_activity:?}, {} {second_activity:?}",
        first.repetition + 1,
        first.label,
        second.label,
    );
    first.status = RunStatus::Failed;
    first.error = Some(error.clone());
    second.status = RunStatus::Failed;
    second.error = Some(error);
}

fn trigger_counts(sample: &RunSample) -> Vec<(&'static str, usize)> {
    sample
        .outcome
        .as_ref()
        .into_iter()
        .flat_map(|outcome| &outcome.analysis_activity)
        .map(|activity| (activity.phase, activity.solar_analysis_triggers))
        .collect()
}

struct BenchmarkSession {
    process: LspProcess,
    fixture: Fixture,
    forge: PathBuf,
    documents: BTreeMap<PathBuf, Document>,
    analysis_triggers: usize,
}

impl BenchmarkSession {
    fn new(process: LspProcess, fixture: Fixture, forge: PathBuf) -> Result<Self> {
        let mut process = process;
        let sentinel_uri = file_uri(&fixture.erc4626_path())?;
        process.observe_solar_diagnostics_for(sentinel_uri.to_string());
        Ok(Self { process, fixture, forge, documents: BTreeMap::new(), analysis_triggers: 0 })
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
                    "workspace": {"workspaceFolders": true}
                },
                "initializationOptions": {"forgePath": self.forge},
                "workspaceFolders": [{"uri": root_uri, "name": "solar-lsp-bench"}]
            }),
        )?;
        self.process.notify("initialized", json!({}))
    }

    fn prepare(&mut self) -> Result<()> {
        let path = self.fixture.erc4626_path();
        let text = fs::read_to_string(&path)?;
        let text = insert_startup_statement(&text)?;
        self.open_document(path.clone(), text)?;
        let uri = file_uri(&path)?;
        self.process
            .wait_for_solar_marker(uri.as_str(), STARTUP_MARKER)
            .context("waiting for startup Solar diagnostics")?;
        Ok(())
    }

    fn reset_measurements(&mut self) {
        self.analysis_triggers = 0;
        self.process.clear_measurements();
    }

    fn user_session(&mut self) -> Result<SessionOutcome> {
        let mut latencies = Vec::new();
        let mut analysis_activity = Vec::new();
        let mut sentinel_marker = STARTUP_MARKER.to_string();

        self.typing_phase(
            "slow",
            SLOW_TYPING_TEXT,
            SLOW_TYPING_INTERVAL,
            &mut sentinel_marker,
            &mut latencies,
            &mut analysis_activity,
        )?;
        self.typing_phase(
            "normal",
            NORMAL_TYPING_TEXT,
            NORMAL_TYPING_INTERVAL,
            &mut sentinel_marker,
            &mut latencies,
            &mut analysis_activity,
        )?;
        self.typing_phase(
            "fast",
            FAST_TYPING_TEXT,
            FAST_TYPING_INTERVAL,
            &mut sentinel_marker,
            &mut latencies,
            &mut analysis_activity,
        )?;

        let cross_file_marker =
            self.navigation_and_cross_file(&mut latencies, &mut analysis_activity)?;
        self.cleanup_and_save(&sentinel_marker, &cross_file_marker, &mut latencies)?;

        Ok(SessionOutcome { latencies, analysis_activity })
    }

    fn typing_phase(
        &mut self,
        phase: &'static str,
        text: &str,
        interval: Duration,
        marker: &mut String,
        latencies: &mut Vec<LatencyMeasurement>,
        analysis_activity: &mut Vec<AnalysisActivity>,
    ) -> Result<()> {
        let activity_start = self.activity_snapshot();
        let path = self.fixture.erc4626_path();
        self.type_after_marker(&path, marker, text, interval)?;
        let uri = file_uri(&path)?;
        let elapsed_ms = self
            .process
            .wait_for_solar_marker(uri.as_str(), marker)
            .with_context(|| format!("waiting for `{phase}` Solar diagnostics"))?;
        latencies.push(LatencyMeasurement {
            name: match phase {
                "slow" => "edit_settle_ms{slow}",
                "normal" => "edit_settle_ms{normal}",
                "fast" => "edit_settle_ms{fast}",
                _ => unreachable!(),
            },
            elapsed_ms,
        });
        analysis_activity.push(self.finish_activity(phase, activity_start)?);
        Ok(())
    }

    fn navigation_and_cross_file(
        &mut self,
        latencies: &mut Vec<LatencyMeasurement>,
        analysis_activity: &mut Vec<AnalysisActivity>,
    ) -> Result<String> {
        let sentinel_path = self.fixture.erc4626_path();
        let sentinel_uri = file_uri(&sentinel_path)?;
        let (completion_position, symbol_position) = {
            let text = &self.documents[&sentinel_path].text;
            symbol_positions(text)?
        };

        let completion = self.process.request(
            "textDocument/completion",
            json!({"textDocument": {"uri": sentinel_uri}, "position": completion_position}),
        )?;
        validate_completion(completion)?;

        let definition = self.process.request(
            "textDocument/definition",
            json!({"textDocument": {"uri": sentinel_uri}, "position": symbol_position}),
        )?;
        let target_path = self.fixture.fixed_point_math_lib_path();
        let target_uri = file_uri(&target_path)?;
        validate_definition(definition, &target_uri)?;

        let activity_start = self.activity_snapshot();
        let target_text = fs::read_to_string(&target_path)?;
        let target_text = insert_statement_after_function_open(
            &target_text,
            FULL_MUL_DIV_DECLARATION,
            CROSS_FILE_MARKER,
        )?;
        self.open_document(target_path.clone(), target_text)?;

        let end = end_position(&self.documents[&target_path].text);
        let hints = self.process.request(
            "textDocument/inlayHint",
            json!({
                "textDocument": {"uri": target_uri},
                "range": {"start": {"line": 0, "character": 0}, "end": end}
            }),
        )?;
        validate_inlay_hints(hints)?;

        let mut cross_file_marker = CROSS_FILE_MARKER.to_string();
        self.type_after_marker(
            &target_path,
            &mut cross_file_marker,
            CROSS_FILE_TYPING_TEXT,
            FAST_TYPING_INTERVAL,
        )?;
        let elapsed_ms = self
            .process
            .wait_for_solar_marker(target_uri.as_str(), &cross_file_marker)
            .context("waiting for cross-file Solar diagnostics")?;
        latencies.push(LatencyMeasurement { name: "edit_settle_ms{cross-file}", elapsed_ms });
        analysis_activity.push(self.finish_activity("navigation-cross-file", activity_start)?);

        let references = self.process.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": sentinel_uri},
                "position": symbol_position,
                "context": {"includeDeclaration": true}
            }),
        )?;
        validate_references(references, &target_uri, &sentinel_uri, symbol_position)?;

        Ok(cross_file_marker)
    }

    fn cleanup_and_save(
        &mut self,
        sentinel_marker: &str,
        cross_file_marker: &str,
        latencies: &mut Vec<LatencyMeasurement>,
    ) -> Result<()> {
        let target_path = self.fixture.fixed_point_math_lib_path();
        self.remove_marker_statement(&target_path, cross_file_marker)?;

        let sentinel_path = self.fixture.erc4626_path();
        self.remove_marker_statement(&sentinel_path, sentinel_marker)?;
        self.rename_supply_for_forge(&sentinel_path)?;

        let sentinel_uri = file_uri(&sentinel_path)?;
        self.process
            .wait_for_no_solar_diagnostics(sentinel_uri.as_str())
            .context("waiting for cleanup Solar diagnostics")?;
        for (path, document) in &self.documents {
            if document.text.contains(STARTUP_MARKER) || document.text.contains(CROSS_FILE_MARKER) {
                bail!("benchmark marker remains in `{}` after cleanup", path.display())
            }
            fs::write(path, &document.text)?;
        }

        self.process
            .notify("textDocument/didSave", json!({"textDocument": {"uri": sentinel_uri}}))?;
        let elapsed_ms = self
            .process
            .wait_for_diagnostic(
                sentinel_uri.as_str(),
                FORGE_DIAGNOSTIC_SOURCE,
                FORGE_DIAGNOSTIC_CODE,
            )
            .context("waiting for Forge lint diagnostics")?;
        latencies.push(LatencyMeasurement { name: "forge_flycheck_ready_ms", elapsed_ms });
        Ok(())
    }

    fn open_document(&mut self, path: PathBuf, text: String) -> Result<()> {
        if self.documents.contains_key(&path) {
            return Ok(());
        }
        let uri = file_uri(&path)?;
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
        self.analysis_triggers += 1;
        self.documents.insert(path, Document { text, version: 1 });
        Ok(())
    }

    fn type_after_marker(
        &mut self,
        path: &Path,
        marker: &mut String,
        text: &str,
        interval: Duration,
    ) -> Result<()> {
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            self.append_to_marker(path, marker, character)?;
            if characters.peek().is_some() {
                thread::sleep(interval);
            }
        }
        Ok(())
    }

    fn append_to_marker(
        &mut self,
        path: &Path,
        marker: &mut String,
        character: char,
    ) -> Result<()> {
        let offset = self.documents[path]
            .text
            .find(marker.as_str())
            .map(|offset| offset + marker.len())
            .with_context(|| format!("marker `{marker}` is missing from `{}`", path.display()))?;
        self.replace_range(path, offset..offset, &character.to_string())?;
        marker.push(character);
        Ok(())
    }

    fn remove_marker_statement(&mut self, path: &Path, marker: &str) -> Result<()> {
        let text = &self.documents[path].text;
        let marker_offset = unique_offset(text, marker)?;
        let line_start = text[..marker_offset].rfind('\n').map_or(0, |index| index + 1);
        let line_end =
            text[marker_offset..].find('\n').map_or(text.len(), |index| marker_offset + index + 1);
        if text[line_start..line_end].trim() != format!("{marker};") {
            bail!("marker `{marker}` is not a standalone statement in `{}`", path.display())
        }
        self.replace_range(path, line_start..line_end, "")
    }

    fn rename_supply_for_forge(&mut self, path: &Path) -> Result<()> {
        let updated = rename_supply(&self.documents[path].text)?;
        self.replace_document(path, updated)
    }

    fn replace_range(&mut self, path: &Path, range: Range<usize>, replacement: &str) -> Result<()> {
        let uri = file_uri(path)?;
        let (version, start, end) = {
            let document = self.documents.get_mut(path).context("document is not open")?;
            let start = position_at(&document.text, range.start);
            let end = position_at(&document.text, range.end);
            document.text.replace_range(range, replacement);
            document.version += 1;
            (document.version, start, end)
        };
        self.process.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"range": {"start": start, "end": end}, "text": replacement}]
            }),
        )?;
        self.analysis_triggers += 1;
        Ok(())
    }

    fn replace_document(&mut self, path: &Path, text: String) -> Result<()> {
        let uri = file_uri(path)?;
        let version = {
            let document = self.documents.get_mut(path).context("document is not open")?;
            document.text.clone_from(&text);
            document.version += 1;
            document.version
        };
        self.process.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"text": text}]
            }),
        )?;
        self.analysis_triggers += 1;
        Ok(())
    }

    fn activity_snapshot(&self) -> (usize, usize) {
        (self.analysis_triggers, self.process.solar_diagnostic_publications())
    }

    fn finish_activity(
        &self,
        phase: &'static str,
        (trigger_start, publication_start): (usize, usize),
    ) -> Result<AnalysisActivity> {
        let solar_analysis_triggers = self
            .analysis_triggers
            .checked_sub(trigger_start)
            .context("analysis trigger counter moved backwards")?;
        let solar_diagnostic_publications = self
            .process
            .solar_diagnostic_publications()
            .checked_sub(publication_start)
            .context("diagnostic publication counter moved backwards")?;
        if solar_diagnostic_publications > solar_analysis_triggers {
            bail!(
                "phase `{phase}` published {solar_diagnostic_publications} Solar diagnostics for {solar_analysis_triggers} analysis triggers"
            )
        }
        Ok(AnalysisActivity { phase, solar_analysis_triggers, solar_diagnostic_publications })
    }
}

struct Document {
    text: String,
    version: i32,
}

fn insert_startup_statement(contents: &str) -> Result<String> {
    let call_offset = unique_offset(contents, FULL_MUL_DIV_CALL)?;
    let declaration = "uint256 supply = totalSupply();";
    let declaration_offset = contents[..call_offset]
        .rfind(declaration)
        .context("`supply` declaration is missing before the benchmark call")?;
    let line_start = contents[..declaration_offset].rfind('\n').map_or(0, |index| index + 1);
    let line_prefix = &contents[line_start..declaration_offset];
    let indentation = &line_prefix[..line_prefix.len() - line_prefix.trim_start().len()];
    let mut output = contents.to_string();
    output.insert_str(line_start, &format!("{indentation}{STARTUP_MARKER};\n"));
    Ok(output)
}

fn insert_statement_after_function_open(
    contents: &str,
    anchor: &str,
    marker: &str,
) -> Result<String> {
    let anchor_offset = unique_offset(contents, anchor)?;
    let brace_offset = contents[anchor_offset..]
        .find('{')
        .map(|offset| anchor_offset + offset)
        .context("function declaration has no body")?;
    let mut output = contents.to_string();
    output.insert_str(brace_offset + 1, &format!("\n        {marker};"));
    Ok(output)
}

fn symbol_positions(text: &str) -> Result<(Position, Position)> {
    let call_offset = unique_offset(text, FULL_MUL_DIV_CALL)?;
    let completion_offset = call_offset + "FixedPointMathLib.".len();
    let symbol_offset = completion_offset + 1;
    Ok((position_at(text, completion_offset), position_at(text, symbol_offset)))
}

fn rename_supply(text: &str) -> Result<String> {
    let call_offset = unique_offset(text, FULL_MUL_DIV_CALL)?;
    let declaration = "uint256 supply = totalSupply();";
    let declaration_offset = text[..call_offset]
        .rfind(declaration)
        .context("`supply` declaration is missing before the benchmark call")?;
    let mut updated = text.to_string();
    updated.replace_range(
        declaration_offset..declaration_offset + declaration.len(),
        "uint256 total_supply = totalSupply();",
    );

    let call_offset = unique_offset(&updated, FULL_MUL_DIV_CALL)?;
    let condition = "_eitherIsZero(assets, supply)";
    let condition_offset = updated[..call_offset]
        .rfind(condition)
        .context("`supply` condition use is missing before the benchmark call")?;
    updated.replace_range(
        condition_offset..condition_offset + condition.len(),
        "_eitherIsZero(assets, total_supply)",
    );

    let call_offset = unique_offset(&updated, FULL_MUL_DIV_CALL)?;
    let supply_offset = call_offset + "FixedPointMathLib.fullMulDiv(assets, ".len();
    updated.replace_range(supply_offset..supply_offset + "supply".len(), "total_supply");
    Ok(updated)
}

fn validate_completion(value: Value) -> Result<()> {
    let response = serde_json::from_value::<CompletionResponse>(value)
        .context("completion response has an unexpected shape")?;
    let items = match response {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    if !items.iter().any(|item| item.label == FULL_MUL_DIV_NAME) {
        bail!("completion response does not contain `{FULL_MUL_DIV_NAME}`")
    }
    Ok(())
}

fn validate_definition(value: Value, target_uri: &Url) -> Result<()> {
    let response = serde_json::from_value::<GotoDefinitionResponse>(value)
        .context("definition response has an unexpected shape")?;
    let found = match response {
        GotoDefinitionResponse::Scalar(location) => location.uri == *target_uri,
        GotoDefinitionResponse::Array(locations) => {
            locations.iter().any(|location| location.uri == *target_uri)
        }
        GotoDefinitionResponse::Link(links) => {
            links.iter().any(|location| location.target_uri == *target_uri)
        }
    };
    if !found {
        bail!("definition response does not target `{target_uri}`")
    }
    Ok(())
}

fn validate_inlay_hints(value: Value) -> Result<()> {
    let hints = serde_json::from_value::<Vec<InlayHint>>(value)
        .context("inlay hint response has an unexpected shape")?;
    if hints.is_empty() {
        bail!("inlay hint response is empty")
    }
    Ok(())
}

fn validate_references(
    value: Value,
    declaration_uri: &Url,
    call_uri: &Url,
    call_position: Position,
) -> Result<()> {
    let locations = serde_json::from_value::<Vec<Location>>(value)
        .context("references response has an unexpected shape")?;
    if !locations.iter().any(|location| location.uri == *declaration_uri) {
        bail!("references response does not contain the declaration in `{declaration_uri}`")
    }
    if !locations.iter().any(|location| {
        location.uri == *call_uri && position_in_range(call_position, location.range)
    }) {
        bail!("references response does not contain the known call in `{call_uri}`")
    }
    Ok(())
}

fn position_in_range(position: Position, range: lsp_types::Range) -> bool {
    position_key(range.start) <= position_key(position)
        && position_key(position) <= position_key(range.end)
}

fn position_key(position: Position) -> (u32, u32) {
    (position.line, position.character)
}

fn unique_offset(contents: &str, anchor: &str) -> Result<usize> {
    let mut offsets = contents.match_indices(anchor).map(|(offset, _)| offset);
    let offset = offsets.next().with_context(|| format!("anchor `{anchor}` is missing"))?;
    if offsets.next().is_some() {
        bail!("anchor `{anchor}` is not unique")
    }
    Ok(offset)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_phases_use_eight_ascii_characters_and_human_intervals() {
        for text in [SLOW_TYPING_TEXT, NORMAL_TYPING_TEXT, FAST_TYPING_TEXT, CROSS_FILE_TYPING_TEXT]
        {
            assert_eq!(text.len(), 8);
            assert!(text.is_ascii());
        }
        assert_eq!(SLOW_TYPING_INTERVAL, Duration::from_millis(400));
        assert_eq!(NORMAL_TYPING_INTERVAL, Duration::from_millis(240));
        assert_eq!(FAST_TYPING_INTERVAL, Duration::from_millis(120));
    }

    #[test]
    fn comparison_plan_alternates_binary_order() {
        let baseline = PathBuf::from("baseline");
        let candidate = PathBuf::from("candidate");
        let plan = comparison_plan(&baseline, &candidate, 3);
        let labels = plan.iter().map(|run| run.label).collect::<Vec<_>>();

        assert_eq!(
            labels,
            ["baseline", "candidate", "candidate", "baseline", "baseline", "candidate"]
        );
    }

    #[test]
    fn trigger_mismatch_fails_both_runs_in_a_pair() {
        let mut pair = [trigger_sample("baseline", 8), trigger_sample("candidate", 7)];

        validate_pair(&mut pair);

        assert!(!pair[0].succeeded());
        assert!(!pair[1].succeeded());
        assert!(pair[0].error.as_deref().unwrap().contains("trigger counts differ"));
        assert_eq!(pair[0].error, pair[1].error);
    }

    #[test]
    fn benchmark_markers_are_inserted_as_removable_statements() {
        let source = "function f() {\n    uint256 supply = totalSupply();\n    return FixedPointMathLib.fullMulDiv(assets, supply, totalAssets());\n}\n";
        let inserted = insert_startup_statement(source).unwrap();
        assert_eq!(
            inserted,
            "function f() {\n    solar_bench_startup;\n    uint256 supply = totalSupply();\n    return FixedPointMathLib.fullMulDiv(assets, supply, totalAssets());\n}\n"
        );

        let source = "function fullMulDiv(uint x) returns (uint) { return x; }\n";
        let inserted = insert_statement_after_function_open(
            source,
            FULL_MUL_DIV_DECLARATION,
            CROSS_FILE_MARKER,
        )
        .unwrap();
        assert!(inserted.contains("{\n        solar_bench_cross_file; return x;"));
    }

    #[test]
    fn cleanup_renames_the_selected_supply_and_all_of_its_uses() {
        let source = "function f() {\n    uint256 supply = totalSupply();\n    return _eitherIsZero(assets, supply)\n        ? 0\n        : FixedPointMathLib.fullMulDiv(assets, supply, totalAssets());\n}\n";

        let renamed = rename_supply(source).unwrap();

        assert!(renamed.contains("uint256 total_supply = totalSupply();"));
        assert!(renamed.contains("_eitherIsZero(assets, total_supply)"));
        assert!(
            renamed.contains("FixedPointMathLib.fullMulDiv(assets, total_supply, totalAssets())")
        );
        assert!(!renamed.contains("assets, supply"));
    }

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

    fn trigger_sample(label: &'static str, solar_analysis_triggers: usize) -> RunSample {
        RunSample {
            label,
            binary: label.into(),
            repetition: 0,
            status: RunStatus::Ok,
            outcome: Some(SessionOutcome {
                latencies: Vec::new(),
                analysis_activity: vec![AnalysisActivity {
                    phase: "slow",
                    solar_analysis_triggers,
                    solar_diagnostic_publications: 1,
                }],
            }),
            process: None,
            observations: Observations::default(),
            error: None,
        }
    }
}
