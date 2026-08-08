//! Manifest-driven benchmark execution.

use crate::{
    config::{Config, ProbeSpec, ProfileSpec, ServerSpec, StepSpec, WorkloadSpec},
    fixture::{
        Anchor, Fixture, FixtureMetadata, FixtureSource, PositionEncoding, file_uri,
        offset_at_position, position_at_with_encoding,
    },
    lifecycle::{
        VERSION_PROBE_TIMEOUT, inspect_version, resolve_executable, verify_server_version_output,
    },
    process::{FinishedProcess, LspProcess, Observations, ProcessEnvironment, RemoteError},
    report::{
        CorrectnessResult, ProcessPhase, RunSample, RunStatus, ServerMetadata, ServerStatus,
        SummaryInput, SummaryReport, summarize, write_reports,
    },
};
use anyhow::{Context, Result, anyhow, bail};
use lsp_types::{CompletionResponse, GotoDefinitionResponse, Location, Range, Url};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub(crate) struct RunOptions {
    pub(crate) config: PathBuf,
    pub(crate) repeat: usize,
    pub(crate) timeout: Duration,
    pub(crate) profile: String,
    pub(crate) output: PathBuf,
    pub(crate) servers: BTreeSet<String>,
    pub(crate) workloads: BTreeSet<String>,
}

pub(crate) struct RunOutcome {
    pub(crate) summary: SummaryReport,
    pub(crate) failed_runs: usize,
}

struct PreparedServer {
    spec: ServerSpec,
    metadata: ServerMetadata,
}

struct PreparedFixture {
    source: FixtureSource,
}

#[derive(Clone, Copy, Debug)]
enum FailureKind {
    Unsupported,
    Incorrect,
    Timeout,
    Crashed,
    HarnessError,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct WorkloadError {
    kind: FailureKind,
    message: String,
}

impl WorkloadError {
    fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

pub(crate) fn run(options: RunOptions) -> Result<RunOutcome> {
    let config = Config::load(&options.config)?;
    let profile = config
        .profiles
        .get(&options.profile)
        .with_context(|| format!("benchmark profile `{}` is not defined", options.profile))?;
    let repeat_override = (options.repeat != 0).then_some(options.repeat);
    let timeout = if options.timeout.is_zero() {
        Duration::from_millis(profile.timeout_ms)
    } else {
        options.timeout
    };
    let servers = config
        .servers
        .iter()
        .filter(|server| options.servers.is_empty() || options.servers.contains(&server.id))
        .map(prepare_server)
        .collect::<Result<Vec<_>>>()?;
    if servers.is_empty() {
        bail!("the selected benchmark config contains no servers")
    }

    let mut fixtures = BTreeMap::new();
    let mut fixture_metadata = Vec::new();
    for spec in &config.fixtures {
        if !spec.enabled {
            continue;
        }
        match FixtureSource::open(spec) {
            Ok(source) => {
                fixture_metadata.push(source.metadata().clone());
                fixtures.insert(spec.id.clone(), PreparedFixture { source });
            }
            Err(error) => {
                fixture_metadata.push(FixtureMetadata {
                    id: spec.id.clone(),
                    root: spec.root.clone(),
                    revision: spec.revision.clone(),
                    source_file_count: 0,
                    source_line_count: 0,
                    source_byte_count: 0,
                    content_sha256: "unavailable".into(),
                    corpus: spec.corpus.clone(),
                    source: spec.source.clone(),
                    solc: spec.solc.clone(),
                    solc_native_sha256: None,
                    solc_soljson_sha256: None,
                    foundry: spec.foundry.clone(),
                    foundry_native_sha256: None,
                    dependencies: spec.dependencies.clone(),
                });
                eprintln!("fixture {} unavailable: {error:#}", spec.id);
            }
        }
    }

    let workloads = config
        .workloads
        .iter()
        .filter(|workload| {
            options.workloads.is_empty()
                && (profile.scenarios.is_empty() || profile.scenarios.contains(&workload.id))
                || options.workloads.contains(&workload.id)
        })
        .collect::<Vec<_>>();
    if workloads.is_empty() {
        bail!("the selected benchmark config contains no workloads")
    }

    let mut samples = Vec::new();
    let mut workload_repetitions = BTreeMap::new();
    for workload in workloads {
        let repeat = repeat_override.unwrap_or_else(|| repetitions_for(profile, workload));
        workload_repetitions.insert(workload.id.clone(), repeat);
        let Some(fixture) = fixtures.get(&workload.fixture) else {
            for server in &servers {
                for repetition in 0..repeat {
                    samples.push(unavailable_sample(
                        &server.metadata.id,
                        &workload.fixture,
                        &workload.id,
                        repetition,
                        RunStatus::Unavailable,
                        "fixture is unavailable",
                    ));
                }
            }
            continue;
        };
        for repetition in 0..repeat {
            for index in 0..servers.len() {
                let server = &servers[(index + repetition) % servers.len()];
                let sample = if server.metadata.status != ServerStatus::Available {
                    unavailable_sample(
                        &server.metadata.id,
                        &workload.fixture,
                        &workload.id,
                        repetition,
                        match server.metadata.status {
                            ServerStatus::Incompatible => RunStatus::Incompatible,
                            _ => RunStatus::Unavailable,
                        },
                        server.metadata.error.as_deref().unwrap_or("server is unavailable"),
                    )
                } else {
                    eprintln!(
                        "{} {}/{} repetition {}",
                        server.metadata.id,
                        workload.fixture,
                        workload.id,
                        repetition + 1
                    );
                    run_once(server, &fixture.source, workload, profile, repetition, timeout)
                };
                samples.push(sample);
            }
        }
    }

    let summary = summarize(SummaryInput {
        config_path: options.config.clone(),
        config: &config,
        servers: servers.into_iter().map(|server| server.metadata).collect(),
        fixtures: fixture_metadata,
        samples: &samples,
        repeat_override,
        workload_repetitions: &workload_repetitions,
        timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
        profile: options.profile.clone(),
    });
    write_reports(&options.output, &summary, &samples)?;
    let failed_runs = samples
        .iter()
        .filter(|sample| !matches!(sample.status, RunStatus::Pass | RunStatus::Unsupported))
        .count();
    Ok(RunOutcome { summary, failed_runs })
}

fn repetitions_for(profile: &ProfileSpec, workload: &WorkloadSpec) -> usize {
    if workload.steps.iter().any(|step| matches!(step, StepSpec::Warm { .. })) {
        profile.cold_samples
    } else {
        profile.lifecycle_samples
    }
}

fn prepare_server(spec: &ServerSpec) -> Result<PreparedServer> {
    let command = resolve_executable(&spec.command);
    let mut metadata = ServerMetadata {
        id: spec.id.clone(),
        label: spec.label.clone(),
        command: command.clone(),
        args: spec.args.clone(),
        version: None,
        locked_version: spec.locked_version.clone(),
        source: spec.source.clone(),
        executable_sha256: command
            .is_file()
            .then(|| crate::lifecycle::sha256_path(&command).ok())
            .flatten(),
        artifact_path: spec.artifact.as_ref().map(|artifact| artifact.path.clone()),
        artifact_expected_sha256: spec
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.sha256.clone()),
        artifact_sha256: spec
            .artifact
            .as_ref()
            .and_then(|artifact| crate::lifecycle::sha256_path(&artifact.path).ok()),
        required: spec.required,
        status: if spec.enabled { ServerStatus::Unavailable } else { ServerStatus::Disabled },
        error: None,
    };
    if !spec.enabled {
        return Ok(PreparedServer { spec: spec.clone(), metadata });
    }
    if !command.is_absolute() && command.components().count() > 1
        || command.is_absolute() && !command.is_file()
    {
        metadata.error = Some(format!("server executable `{}` was not found", command.display()));
        return Ok(PreparedServer { spec: spec.clone(), metadata });
    }
    let version = inspect_version(&command, spec, VERSION_PROBE_TIMEOUT);
    match version {
        Ok(version) => {
            match verify_server_version_output(spec, &version) {
                Ok(()) => metadata.status = ServerStatus::Available,
                Err(error) => {
                    metadata.status = ServerStatus::Incompatible;
                    metadata.error = Some(format!("{error:#}"));
                }
            }
            metadata.version = Some(version);
        }
        Err(error) => metadata.error = Some(format!("version probe failed: {error:#}")),
    }
    Ok(PreparedServer { spec: spec.clone(), metadata })
}

fn run_once(
    server: &PreparedServer,
    fixture_source: &FixtureSource,
    workload: &WorkloadSpec,
    profile: &ProfileSpec,
    repetition: usize,
    timeout: Duration,
) -> RunSample {
    let mut sample = RunSample {
        server: server.metadata.id.clone(),
        fixture: workload.fixture.clone(),
        workload: workload.id.clone(),
        repetition,
        status: RunStatus::HarnessError,
        timings_ms: BTreeMap::new(),
        process: None,
        setup_phases: Vec::new(),
        observations: Observations::default(),
        correctness: Vec::new(),
        error: None,
    };
    let fixture = match fixture_source.materialize() {
        Ok(fixture) => fixture,
        Err(error) => return sample_with_error(sample, FailureKind::HarnessError, error),
    };
    let environment = match ProcessEnvironment::for_toolchains(
        fixture.metadata().solc.as_ref(),
        fixture.metadata().foundry.as_ref(),
        profile.network_isolation,
    ) {
        Ok(environment) => environment,
        Err(error) => return sample_with_error(sample, FailureKind::HarnessError, error),
    };
    let restart = workload.steps.iter().position(|step| matches!(step, StepSpec::Restart { .. }));
    let measured_steps = if let Some(index) = restart {
        let setup = run_phase(
            &server.spec,
            &fixture,
            &workload.steps[..index],
            profile,
            timeout,
            environment.clone(),
        );
        if !record_setup_phase(&mut sample, setup) {
            return sample;
        }
        if let StepSpec::Restart { invalidate: Some(invalidate) } = &workload.steps[index]
            && let Err(error) = invalidate_fixture(&fixture, invalidate)
        {
            return sample_with_error(sample, FailureKind::HarnessError, error);
        }
        &workload.steps[index + 1..]
    } else {
        &workload.steps
    };
    let measured = run_phase(&server.spec, &fixture, measured_steps, profile, timeout, environment);
    record_measured_phase(&mut sample, measured);
    sample
}

struct PhaseOutcome {
    result: Result<()>,
    process_result: Result<FinishedProcess>,
    timings: BTreeMap<String, f64>,
    correctness: Vec<CorrectnessResult>,
    fallback_observations: Observations,
}

fn run_phase(
    server: &ServerSpec,
    fixture: &Fixture,
    steps: &[StepSpec],
    profile: &ProfileSpec,
    timeout: Duration,
    environment: ProcessEnvironment,
) -> PhaseOutcome {
    let process =
        match LspProcess::spawn_with_environment(server, fixture.root(), timeout, environment) {
            Ok(process) => process,
            Err(error) => {
                return PhaseOutcome {
                    result: Err(error),
                    process_result: Err(anyhow!("server did not start")),
                    timings: BTreeMap::new(),
                    correctness: Vec::new(),
                    fallback_observations: Observations::default(),
                };
            }
        };
    let mut session = Session::new(process, fixture);
    let result = session.initialize().and_then(|()| session.execute(steps, profile));
    let graceful = result.is_ok()
        || result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<WorkloadError>())
            .is_some_and(|error| {
                !matches!(error.kind, FailureKind::Timeout | FailureKind::Crashed)
            });
    let fallback_observations = session.process.observations().clone();
    let process_result = session.process.finish(graceful);
    PhaseOutcome {
        result,
        process_result,
        timings: session.timings,
        correctness: session.correctness,
        fallback_observations,
    }
}

fn record_setup_phase(sample: &mut RunSample, outcome: PhaseOutcome) -> bool {
    let PhaseOutcome { result, process_result, timings, correctness, fallback_observations } =
        outcome;
    sample.correctness.extend(correctness.into_iter().map(|mut result| {
        result.probe = format!("cache-setup/{}", result.probe);
        result
    }));
    for (name, value) in timings {
        sample.timings_ms.insert(format!("cache_setup_{name}"), value);
    }
    match (result, process_result) {
        (Ok(()), Ok(FinishedProcess { metrics, observations })) if metrics.exit_code == Some(0) => {
            sample.timings_ms.insert("cache_population_process_ms".into(), metrics.wall_ms);
            sample.setup_phases.push(ProcessPhase {
                name: "cache-population".into(),
                process: metrics,
                observations,
            });
            true
        }
        (Err(error), Ok(FinishedProcess { metrics, observations })) => {
            let kind = error
                .downcast_ref::<WorkloadError>()
                .map_or(FailureKind::HarnessError, |error| error.kind);
            sample.status = status_for(kind);
            sample.error = Some(format!("cache setup failed: {error:#}"));
            sample.setup_phases.push(ProcessPhase {
                name: "cache-population".into(),
                process: metrics,
                observations,
            });
            false
        }
        (Ok(()), Ok(FinishedProcess { metrics, observations })) => {
            sample.status = RunStatus::Crash;
            sample.error = Some(format!("cache setup server exited with {:?}", metrics.exit_code));
            sample.setup_phases.push(ProcessPhase {
                name: "cache-population".into(),
                process: metrics,
                observations,
            });
            false
        }
        (result, Err(stop_error)) => {
            sample.status = RunStatus::HarnessError;
            sample.error = Some(match result {
                Ok(()) => format!("failed to stop cache setup server: {stop_error:#}"),
                Err(error) => {
                    format!("cache setup failed: {error:#}; failed to stop server: {stop_error:#}")
                }
            });
            sample.observations = fallback_observations;
            false
        }
    }
}

fn record_measured_phase(sample: &mut RunSample, outcome: PhaseOutcome) {
    let PhaseOutcome { result, process_result, timings, correctness, fallback_observations } =
        outcome;
    sample.correctness.extend(correctness);
    sample.timings_ms.extend(timings);
    match (result, process_result) {
        (Ok(()), Ok(FinishedProcess { metrics, observations })) if metrics.exit_code == Some(0) => {
            sample.status = RunStatus::Pass;
            sample.process = Some(metrics);
            sample.observations = observations;
        }
        (Err(error), Ok(FinishedProcess { metrics, observations })) => {
            let kind = error
                .downcast_ref::<WorkloadError>()
                .map_or(FailureKind::HarnessError, |error| error.kind);
            sample.status = status_for(kind);
            sample.error = Some(format!("{error:#}"));
            sample.process = Some(metrics);
            sample.observations = observations;
        }
        (Ok(()), Ok(FinishedProcess { metrics, observations })) => {
            sample.status = RunStatus::Crash;
            sample.error = Some(format!("server exited with {:?}", metrics.exit_code));
            sample.process = Some(metrics);
            sample.observations = observations;
        }
        (result, Err(stop_error)) => {
            sample.status = RunStatus::HarnessError;
            sample.error = Some(match result {
                Ok(()) => format!("failed to stop server: {stop_error:#}"),
                Err(error) => format!("{error:#}; failed to stop server: {stop_error:#}"),
            });
            sample.observations = fallback_observations;
        }
    }
}

fn invalidate_fixture(
    fixture: &Fixture,
    replacement: &crate::config::DiskReplacementSpec,
) -> Result<()> {
    let path = fixture.path(&replacement.path)?;
    let anchor = fixture.anchor(&replacement.anchor)?;
    if anchor.path != path {
        bail!(
            "restart invalidation anchor `{}` belongs to `{}`",
            replacement.anchor,
            anchor.path.display()
        )
    }
    let needle = fixture.anchor_needle(&replacement.anchor)?;
    let mut text = fs::read_to_string(&path)?;
    let start = text.find(&needle).with_context(|| {
        format!("restart invalidation anchor `{}` disappeared", replacement.anchor)
    })?;
    text.replace_range(start..start + needle.len(), &replacement.text);
    atomic_write(&path, &text, 0)
}

fn sample_with_error(mut sample: RunSample, kind: FailureKind, error: anyhow::Error) -> RunSample {
    sample.status = status_for(kind);
    sample.error = Some(format!("{error:#}"));
    sample
}

fn unavailable_sample(
    server: &str,
    fixture: &str,
    workload: &str,
    repetition: usize,
    status: RunStatus,
    error: &str,
) -> RunSample {
    RunSample {
        server: server.into(),
        fixture: fixture.into(),
        workload: workload.into(),
        repetition,
        status,
        timings_ms: BTreeMap::new(),
        process: None,
        setup_phases: Vec::new(),
        observations: Observations::default(),
        correctness: Vec::new(),
        error: Some(error.into()),
    }
}

fn status_for(kind: FailureKind) -> RunStatus {
    match kind {
        FailureKind::Unsupported => RunStatus::Unsupported,
        FailureKind::Incorrect => RunStatus::Incorrect,
        FailureKind::Timeout => RunStatus::Timeout,
        FailureKind::Crashed => RunStatus::Crash,
        FailureKind::HarnessError => RunStatus::HarnessError,
    }
}

struct Session<'a> {
    process: LspProcess,
    fixture: &'a Fixture,
    documents: BTreeMap<PathBuf, Document>,
    timings: BTreeMap<String, f64>,
    correctness: Vec<CorrectnessResult>,
    barriers: BTreeMap<String, Instant>,
    last_open_started: Option<Instant>,
    readiness_quiet: Duration,
}

#[derive(Clone)]
struct Document {
    text: String,
    version: i32,
}

impl<'a> Session<'a> {
    fn new(process: LspProcess, fixture: &'a Fixture) -> Self {
        Self {
            process,
            fixture,
            documents: BTreeMap::new(),
            timings: BTreeMap::new(),
            correctness: Vec::new(),
            barriers: BTreeMap::new(),
            last_open_started: None,
            readiness_quiet: Duration::from_millis(50),
        }
    }

    fn initialize(&mut self) -> Result<()> {
        let root_uri = file_uri(self.fixture.root())?;
        self.process.set_root(root_uri.as_str());
        let root_path = self.fixture.root().display().to_string();
        let result = self.process.setup_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "solar-lsp-bench", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": root_uri,
                "rootPath": root_path,
                "capabilities": {
                    "workspace": {
                        "workspaceFolders": true,
                        "configuration": true,
                        "workspaceEdit": {"documentChanges": true}
                    },
                    "textDocument": {
                        "synchronization": {"dynamicRegistration": false, "didSave": true},
                        "definition": {"linkSupport": true},
                        "completion": {"completionItem": {"snippetSupport": false}},
                        "hover": {"contentFormat": ["markdown", "plaintext"]},
                        "rename": {"prepareSupport": true}
                    },
                    "general": {"positionEncodings": ["utf-8", "utf-16", "utf-32"]}
                },
                "initializationOptions": self.process.initialization_options(),
                "workspaceFolders": [{"uri": root_uri, "name": "lsp-bench"}]
            }),
        )?;
        self.process.set_initialize_result(&result);
        self.timings.insert(
            "spawn_to_initialize_response_ms".into(),
            duration_ms(self.process.process_started_at().elapsed()),
        );
        PositionEncoding::parse(self.process.position_encoding())
            .map_err(|error| WorkloadError::new(FailureKind::HarnessError, format!("{error:#}")))?;
        self.process.notify("initialized", json!({}))
    }

    fn execute(&mut self, steps: &[StepSpec], profile: &ProfileSpec) -> Result<()> {
        self.readiness_quiet = Duration::from_millis(profile.readiness_quiet_ms);
        for step in steps {
            match step {
                StepSpec::Open { path } => self.open(path)?,
                StepSpec::Probe { name, probe } => self.probe(name, probe)?,
                StepSpec::Replace { path, anchor, text, probe } => {
                    self.replace(path, anchor, text)?;
                    if let Some(probe) = probe {
                        self.probe("edit-ready", probe)?;
                    }
                }
                StepSpec::Save { path, probe } => {
                    self.save(path)?;
                    if let Some(probe) = probe {
                        self.probe("save-ready", probe)?;
                    }
                }
                StepSpec::Warm { name, probe, warmup, samples } => {
                    self.warm(
                        name,
                        probe,
                        warmup.unwrap_or(profile.warmup),
                        samples.unwrap_or(profile.samples),
                    )
                    .map_err(anyhow::Error::from)?;
                }
                StepSpec::Rename { path, anchor, new_name, probe } => {
                    self.rename_symbol(path, anchor, new_name, probe.as_ref())?;
                }
                StepSpec::CreateFile { path, text, probe } => {
                    self.create_file(path, text, probe.as_ref())?;
                }
                StepSpec::RenameFile { from, to, probe } => {
                    self.rename_file(from, to, probe.as_ref())?;
                }
                StepSpec::DeleteFile { path, probe } => {
                    self.delete_file(path, probe.as_ref())?;
                }
                StepSpec::Restart { .. } => bail!("restart step reached session execution"),
            }
        }
        Ok(())
    }

    fn open(&mut self, relative: &Path) -> Result<()> {
        let path = self.fixture.path(relative)?;
        if self.documents.contains_key(&path) {
            return Ok(());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read `{}`", relative.display()))?;
        let uri = file_uri(&path)?;
        let started = Instant::now();
        self.process.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {"uri": uri, "languageId": "solidity", "version": 1, "text": text}
            }),
        )?;
        self.last_open_started = Some(started);
        self.documents.insert(path, Document { text, version: 1 });
        Ok(())
    }

    fn replace(&mut self, relative: &Path, anchor_name: &str, replacement: &str) -> Result<()> {
        self.open(relative)?;
        let path = self.fixture.path(relative)?;
        let anchor = self.fixture.anchor(anchor_name)?;
        if anchor.path != path {
            return Err(WorkloadError::new(
                FailureKind::HarnessError,
                format!("anchor `{anchor_name}` belongs to `{}`", anchor.path.display()),
            )
            .into());
        }
        let (version, start, end, text) = {
            let document = self.documents.get_mut(&path).context("document is not open")?;
            let needle = self.fixture.anchor_needle(anchor_name)?;
            let offset = document
                .text
                .find(&needle)
                .context("edit anchor disappeared from open document")?;
            let end_offset = offset + needle.len();
            let encoding = PositionEncoding::parse(self.process.position_encoding())?;
            let start = position_at_with_encoding(&document.text, offset, encoding);
            let end = position_at_with_encoding(&document.text, end_offset, encoding);
            document.text.replace_range(offset..end_offset, replacement);
            document.version += 1;
            (document.version, start, end, document.text.clone())
        };
        let uri = file_uri(&path)?;
        self.barriers.insert("edit".into(), Instant::now());
        self.process.send_change(
            uri.as_str(),
            version,
            json!(start),
            json!(end),
            replacement,
            &text,
        )
    }

    fn save(&mut self, relative: &Path) -> Result<()> {
        let path = self.fixture.path(relative)?;
        let document = self.documents.get(&path).context("document is not open")?.clone();
        let uri = file_uri(&path)?;
        let started = Instant::now();
        self.barriers.insert("save".into(), started);
        atomic_write(&path, &document.text, document.version)?;
        self.process.notify(
            "textDocument/didSave",
            json!({
                "textDocument": {"uri": uri, "version": document.version},
                "text": document.text
            }),
        )?;
        self.timings
            .insert(format!("save_{}_ms", relative.display()), duration_ms(started.elapsed()));
        Ok(())
    }

    fn rename_symbol(
        &mut self,
        relative: &Path,
        anchor: &str,
        new_name: &str,
        probe: Option<&ProbeSpec>,
    ) -> Result<()> {
        if !self.process.supports("textDocument/rename") {
            return Err(WorkloadError::new(
                FailureKind::Unsupported,
                "server does not advertise rename",
            )
            .into());
        }
        self.open(relative)?;
        let encoding = PositionEncoding::parse(self.process.position_encoding())?;
        let anchor = self.fixture.anchor_with_encoding(anchor, encoding)?;
        let uri = file_uri(&anchor.path)?;
        let started = Instant::now();
        self.barriers.insert("rename".into(), started);
        let edit = self.process.request(
            "textDocument/rename",
            json!({
                "textDocument": {"uri": uri},
                "position": anchor.position,
                "newName": new_name
            }),
        )?;
        if edit.is_null() {
            return Err(WorkloadError::new(FailureKind::Incorrect, "rename returned null").into());
        }
        let mut applied = self.apply_workspace_edit(&edit)?;
        for edit in self.process.take_workspace_edits() {
            applied += self.apply_workspace_edit(&edit)?;
        }
        if applied == 0 {
            return Err(WorkloadError::new(
                FailureKind::Incorrect,
                "rename WorkspaceEdit changed no files",
            )
            .into());
        }
        if let Some(probe) = probe {
            self.probe("rename-ready", probe)?;
        } else {
            self.timings.insert("rename_end_to_end_ms".into(), duration_ms(started.elapsed()));
            self.barriers.remove("rename");
        }
        Ok(())
    }

    fn create_file(
        &mut self,
        relative: &Path,
        text: &str,
        probe: Option<&ProbeSpec>,
    ) -> Result<()> {
        let path = self.fixture.path(relative)?;
        if path.exists() {
            bail!("lifecycle create target `{}` already exists", relative.display())
        }
        let started = Instant::now();
        self.barriers.insert("create-file".into(), started);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, text, 1)?;
        let uri = file_uri(&path)?;
        self.process.notify("workspace/didCreateFiles", json!({"files": [{"uri": uri}]}))?;
        self.finish_lifecycle("create-file", started, probe)
    }

    fn rename_file(&mut self, from: &Path, to: &Path, probe: Option<&ProbeSpec>) -> Result<()> {
        let old_path = self.fixture.path(from)?;
        let new_path = self.fixture.path(to)?;
        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let started = Instant::now();
        self.barriers.insert("rename-file".into(), started);
        fs::rename(&old_path, &new_path)?;
        if let Some(document) = self.documents.remove(&old_path) {
            self.documents.insert(new_path.clone(), document);
        }
        self.process.notify(
            "workspace/didRenameFiles",
            json!({"files": [{"oldUri": file_uri(&old_path)?, "newUri": file_uri(&new_path)?}]}),
        )?;
        self.finish_lifecycle("rename-file", started, probe)
    }

    fn delete_file(&mut self, relative: &Path, probe: Option<&ProbeSpec>) -> Result<()> {
        let path = self.fixture.path(relative)?;
        let started = Instant::now();
        self.barriers.insert("delete-file".into(), started);
        fs::remove_file(&path)?;
        self.documents.remove(&path);
        self.process
            .notify("workspace/didDeleteFiles", json!({"files": [{"uri": file_uri(&path)?}]}))?;
        self.finish_lifecycle("delete-file", started, probe)
    }

    fn finish_lifecycle(
        &mut self,
        name: &str,
        started: Instant,
        probe: Option<&ProbeSpec>,
    ) -> Result<()> {
        if let Some(probe) = probe {
            self.probe(&format!("{name}-ready"), probe)
        } else {
            self.timings.insert(format!("{name}_end_to_end_ms"), duration_ms(started.elapsed()));
            self.barriers.remove(name);
            Ok(())
        }
    }

    fn apply_workspace_edit(&mut self, edit: &Value) -> Result<usize> {
        let mut edits = Vec::<(Url, Vec<Value>)>::new();
        if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
            for (uri, values) in changes {
                edits.push((uri.parse()?, values.as_array().cloned().unwrap_or_default()));
            }
        }
        if let Some(document_changes) = edit.get("documentChanges").and_then(Value::as_array) {
            for change in document_changes {
                if let Some(uri) = change.pointer("/textDocument/uri").and_then(Value::as_str) {
                    edits.push((
                        uri.parse()?,
                        change.get("edits").and_then(Value::as_array).cloned().unwrap_or_default(),
                    ));
                } else {
                    self.apply_resource_operation(change)?;
                }
            }
        }
        let mut applied = 0;
        for (uri, text_edits) in edits {
            applied += self.apply_text_edits(&uri, &text_edits)?;
        }
        Ok(applied)
    }

    fn apply_text_edits(&mut self, uri: &Url, edits: &[Value]) -> Result<usize> {
        let path =
            uri.to_file_path().map_err(|()| anyhow!("WorkspaceEdit URI `{uri}` is not a file"))?;
        if !path.starts_with(self.fixture.root()) {
            bail!("WorkspaceEdit path `{}` escapes the fixture", path.display())
        }
        let mut document = self
            .documents
            .get(&path)
            .cloned()
            .unwrap_or(Document { text: fs::read_to_string(&path)?, version: 0 });
        let encoding = PositionEncoding::parse(self.process.position_encoding())?;
        apply_text_edits(&mut document.text, edits, encoding)?;
        document.version += 1;
        atomic_write(&path, &document.text, document.version)?;
        if self.documents.contains_key(&path) {
            self.process.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri, "version": document.version},
                    "contentChanges": [{"text": document.text}]
                }),
            )?;
            self.documents.insert(path, document);
        }
        Ok(edits.len())
    }

    fn apply_resource_operation(&mut self, operation: &Value) -> Result<()> {
        match operation.get("kind").and_then(Value::as_str) {
            Some("create") => {
                let uri = operation
                    .get("uri")
                    .and_then(Value::as_str)
                    .context("create URI is missing")?;
                let path =
                    Url::parse(uri)?.to_file_path().map_err(|()| anyhow!("invalid create URI"))?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                atomic_write(&path, "", 0)
            }
            Some("rename") => {
                let old_uri = operation
                    .get("oldUri")
                    .and_then(Value::as_str)
                    .context("old URI is missing")?;
                let new_uri = operation
                    .get("newUri")
                    .and_then(Value::as_str)
                    .context("new URI is missing")?;
                let old_path =
                    Url::parse(old_uri)?.to_file_path().map_err(|()| anyhow!("invalid old URI"))?;
                let new_path =
                    Url::parse(new_uri)?.to_file_path().map_err(|()| anyhow!("invalid new URI"))?;
                fs::rename(old_path, new_path)?;
                Ok(())
            }
            Some("delete") => {
                let uri = operation
                    .get("uri")
                    .and_then(Value::as_str)
                    .context("delete URI is missing")?;
                let path =
                    Url::parse(uri)?.to_file_path().map_err(|()| anyhow!("invalid delete URI"))?;
                fs::remove_file(path)?;
                Ok(())
            }
            Some(kind) => bail!("unsupported WorkspaceEdit resource operation `{kind}`"),
            None => bail!("WorkspaceEdit document change has no text edit or resource kind"),
        }
    }

    fn probe(&mut self, name: &str, probe: &ProbeSpec) -> Result<()> {
        let started = Instant::now();
        let deadline = Instant::now() + self.process.timeout();
        loop {
            let failure = match self.probe_once(probe, false) {
                Ok(()) => {
                    self.process
                        .wait_for_readiness(self.readiness_quiet)
                        .map_err(classify_request_error)?;
                    let elapsed = duration_ms(started.elapsed());
                    self.timings.insert(format!("{name}_ms"), elapsed);
                    for (barrier, barrier_started) in std::mem::take(&mut self.barriers) {
                        self.timings.insert(
                            format!("{barrier}_to_{name}_ms"),
                            duration_ms(barrier_started.elapsed()),
                        );
                    }
                    if name == "cold-ready" {
                        self.timings.insert(
                            "cold_ready_ms".into(),
                            duration_ms(self.process.process_started_at().elapsed()),
                        );
                        if let Some(open_started) = self.last_open_started.take() {
                            self.timings.insert(
                                "did_open_to_semantic_ready_ms".into(),
                                duration_ms(open_started.elapsed()),
                            );
                        }
                    }
                    self.correctness.push(CorrectnessResult {
                        probe: name.into(),
                        ok: true,
                        detail: "matched".into(),
                    });
                    return Ok(());
                }
                Err(error) => error,
            };
            if matches!(failure.kind, FailureKind::Unsupported) {
                self.correctness.push(CorrectnessResult {
                    probe: name.into(),
                    ok: false,
                    detail: failure.message.clone(),
                });
                return Err(failure.into());
            }
            if Instant::now() >= deadline {
                let error = WorkloadError::new(
                    failure.kind,
                    format!("probe `{name}` did not become correct: {}", failure.message),
                );
                self.correctness.push(CorrectnessResult {
                    probe: name.into(),
                    ok: false,
                    detail: error.message.clone(),
                });
                return Err(error.into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn warm(
        &mut self,
        name: &str,
        probe: &ProbeSpec,
        warmup: usize,
        samples: usize,
    ) -> std::result::Result<(), WorkloadError> {
        for _ in 0..warmup {
            self.probe_once(probe, false)?;
        }
        for index in 0..samples {
            let started = Instant::now();
            self.probe_once(probe, true)?;
            self.timings.insert(format!("warm_{name}_{index}_ms"), duration_ms(started.elapsed()));
        }
        Ok(())
    }

    fn probe_once(
        &mut self,
        probe: &ProbeSpec,
        measured: bool,
    ) -> std::result::Result<(), WorkloadError> {
        match probe {
            ProbeSpec::Definition { path, anchor, expected_path, expected_anchor } => {
                if !self.process.supports("textDocument/definition") {
                    return Err(WorkloadError::new(
                        FailureKind::Unsupported,
                        "server does not advertise definition",
                    ));
                }
                let encoding = self.position_encoding()?;
                self.open(path).map_err(harness_error)?;
                let source_anchor =
                    self.fixture.anchor_with_encoding(anchor, encoding).map_err(harness_error)?;
                let expected = self
                    .fixture
                    .anchor_with_encoding(expected_anchor, encoding)
                    .map_err(harness_error)?;
                let uri = file_uri(&source_anchor.path).map_err(harness_error)?;
                let expected_uri =
                    file_uri(&self.fixture.path(expected_path).map_err(harness_error)?)
                        .map_err(harness_error)?;
                let value = self.request(
                    "textDocument/definition",
                    json!({"textDocument": {"uri": uri}, "position": source_anchor.position}),
                    measured,
                )?;
                validate_definition(value, &expected_uri, &expected)
            }
            ProbeSpec::Completion { path, anchor, expected_label } => {
                if !self.process.supports("textDocument/completion") {
                    return Err(WorkloadError::new(
                        FailureKind::Unsupported,
                        "server does not advertise completion",
                    ));
                }
                let encoding = self.position_encoding()?;
                self.open(path).map_err(harness_error)?;
                let source_anchor =
                    self.fixture.anchor_with_encoding(anchor, encoding).map_err(harness_error)?;
                let uri = file_uri(&source_anchor.path).map_err(harness_error)?;
                let value = self.request(
                    "textDocument/completion",
                    json!({
                        "textDocument": {"uri": uri},
                        "position": source_anchor.position,
                        "context": {"triggerKind": 2, "triggerCharacter": "."}
                    }),
                    measured,
                )?;
                validate_completion(value, expected_label)
            }
            ProbeSpec::Hover { path, anchor, expected_text } => {
                if !self.process.supports("textDocument/hover") {
                    return Err(WorkloadError::new(
                        FailureKind::Unsupported,
                        "server does not advertise hover",
                    ));
                }
                let encoding = self.position_encoding()?;
                self.open(path).map_err(harness_error)?;
                let source_anchor =
                    self.fixture.anchor_with_encoding(anchor, encoding).map_err(harness_error)?;
                let uri = file_uri(&source_anchor.path).map_err(harness_error)?;
                let value = self.request(
                    "textDocument/hover",
                    json!({"textDocument": {"uri": uri}, "position": source_anchor.position}),
                    measured,
                )?;
                validate_hover(value, expected_text)
            }
            ProbeSpec::References { path, anchor, min_count } => {
                if !self.process.supports("textDocument/references") {
                    return Err(WorkloadError::new(
                        FailureKind::Unsupported,
                        "server does not advertise references",
                    ));
                }
                let encoding = self.position_encoding()?;
                self.open(path).map_err(harness_error)?;
                let source_anchor =
                    self.fixture.anchor_with_encoding(anchor, encoding).map_err(harness_error)?;
                let uri = file_uri(&source_anchor.path).map_err(harness_error)?;
                let value = self.request(
                    "textDocument/references",
                    json!({
                        "textDocument": {"uri": uri},
                        "position": source_anchor.position,
                        "context": {"includeDeclaration": true}
                    }),
                    measured,
                )?;
                validate_min_count(value, *min_count, "references")
            }
            ProbeSpec::DocumentSymbol { path, min_count, expected_name } => {
                if !self.process.supports("textDocument/documentSymbol") {
                    return Err(WorkloadError::new(
                        FailureKind::Unsupported,
                        "server does not advertise document symbols",
                    ));
                }
                self.open(path).map_err(harness_error)?;
                let uri = file_uri(&self.fixture.path(path).map_err(harness_error)?)
                    .map_err(harness_error)?;
                let value = self.request(
                    "textDocument/documentSymbol",
                    json!({"textDocument": {"uri": uri}}),
                    measured,
                )?;
                validate_document_symbols(value, *min_count, expected_name.as_deref())
            }
        }
    }

    fn position_encoding(&self) -> std::result::Result<PositionEncoding, WorkloadError> {
        PositionEncoding::parse(self.process.position_encoding())
            .map_err(|error| WorkloadError::new(FailureKind::HarnessError, format!("{error:#}")))
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        measured: bool,
    ) -> std::result::Result<Value, WorkloadError> {
        let result = if measured {
            self.process.request(method, params)
        } else {
            self.process.setup_request(method, params)
        };
        result.map_err(classify_request_error)
    }
}

fn atomic_write(path: &Path, text: &str, version: i32) -> Result<()> {
    let temporary = path.with_extension(format!("lsp-bench-{version}.tmp"));
    fs::write(&temporary, text)
        .with_context(|| format!("failed to write temporary file `{}`", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to atomically replace `{}`", path.display()))?;
    Ok(())
}

fn apply_text_edits(text: &mut String, edits: &[Value], encoding: PositionEncoding) -> Result<()> {
    let original = text.clone();
    let mut replacements = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = serde_json::from_value::<Range>(
            edit.get("range").cloned().context("WorkspaceEdit text edit is missing a range")?,
        )?;
        let replacement = edit
            .get("newText")
            .and_then(Value::as_str)
            .context("WorkspaceEdit text edit is missing `newText`")?;
        let start = offset_at_position(&original, range.start, encoding)?;
        let end = offset_at_position(&original, range.end, encoding)?;
        if start > end {
            bail!("WorkspaceEdit range starts after it ends")
        }
        replacements.push((start, end, replacement));
    }
    replacements.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in replacements.windows(2) {
        if pair[0].1 > pair[1].0 {
            bail!("WorkspaceEdit contains overlapping text edits")
        }
    }
    for (start, end, replacement) in replacements.into_iter().rev() {
        text.replace_range(start..end, replacement);
    }
    Ok(())
}

fn harness_error(error: anyhow::Error) -> WorkloadError {
    WorkloadError::new(FailureKind::HarnessError, format!("{error:#}"))
}

fn classify_request_error(error: anyhow::Error) -> WorkloadError {
    if let Some(remote) = error.downcast_ref::<RemoteError>() {
        let kind = match remote.code {
            Some(-32601) => FailureKind::Unsupported,
            Some(-32602) => FailureKind::HarnessError,
            _ => FailureKind::HarnessError,
        };
        return WorkloadError::new(kind, format!("{remote}"));
    }
    let message = format!("{error:#}");
    let kind = if message.contains("timed out") {
        FailureKind::Timeout
    } else if message.contains("LSP stdout closed unexpectedly") {
        FailureKind::Crashed
    } else {
        FailureKind::HarnessError
    };
    WorkloadError::new(kind, message)
}

fn validate_definition(
    value: Value,
    expected_uri: &Url,
    expected: &Anchor,
) -> std::result::Result<(), WorkloadError> {
    let locations = match serde_json::from_value::<GotoDefinitionResponse>(value.clone()) {
        Ok(GotoDefinitionResponse::Scalar(location)) => vec![location],
        Ok(GotoDefinitionResponse::Array(locations)) => locations,
        Ok(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| Location { uri: link.target_uri, range: link.target_selection_range })
            .collect(),
        Err(_) => {
            if let Some(array) = value.as_array() {
                array.iter().filter_map(location_from_value).collect()
            } else {
                Vec::new()
            }
        }
    };
    let matched = locations.iter().any(|location| {
        location.uri == *expected_uri
            && location.range.start <= expected.position
            && expected.position <= location.range.end
    });
    if matched {
        Ok(())
    } else {
        Err(WorkloadError::new(
            FailureKind::Incorrect,
            format!(
                "definition did not target {} at {:?}: {value}",
                expected_uri, expected.position
            ),
        ))
    }
}

fn location_from_value(value: &Value) -> Option<Location> {
    if value.get("uri").is_some() {
        serde_json::from_value(value.clone()).ok()
    } else {
        let uri = value.get("targetUri")?.as_str()?.parse().ok()?;
        let range = serde_json::from_value(value.get("targetRange")?.clone()).ok()?;
        Some(Location { uri, range })
    }
}

fn validate_completion(
    value: Value,
    expected_label: &str,
) -> std::result::Result<(), WorkloadError> {
    let response = serde_json::from_value::<CompletionResponse>(value.clone()).ok();
    let labels = match response {
        Some(CompletionResponse::Array(items)) => {
            items.into_iter().map(|item| item.label).collect::<Vec<_>>()
        }
        Some(CompletionResponse::List(list)) => {
            list.items.into_iter().map(|item| item.label).collect::<Vec<_>>()
        }
        None => Vec::new(),
    };
    if labels.iter().any(|label| label == expected_label) {
        Ok(())
    } else {
        Err(WorkloadError::new(
            FailureKind::Incorrect,
            format!("completion did not contain `{expected_label}`: {value}"),
        ))
    }
}

fn validate_hover(value: Value, expected_text: &str) -> std::result::Result<(), WorkloadError> {
    let Some(contents) = value.get("contents") else {
        return Err(WorkloadError::new(FailureKind::Incorrect, "hover returned null"));
    };
    if contents.to_string().contains(expected_text) {
        Ok(())
    } else {
        Err(WorkloadError::new(
            FailureKind::Incorrect,
            format!("hover did not contain `{expected_text}`: {contents}"),
        ))
    }
}

fn validate_min_count(
    value: Value,
    min_count: usize,
    description: &str,
) -> std::result::Result<(), WorkloadError> {
    let count = value.as_array().map_or(0, Vec::len);
    if count >= min_count {
        Ok(())
    } else {
        Err(WorkloadError::new(
            FailureKind::Incorrect,
            format!("{description} returned {count} items; expected at least {min_count}: {value}"),
        ))
    }
}

fn validate_document_symbols(
    value: Value,
    min_count: usize,
    expected_name: Option<&str>,
) -> std::result::Result<(), WorkloadError> {
    let count = document_symbol_count(&value);
    if count < min_count {
        return Err(WorkloadError::new(
            FailureKind::Incorrect,
            format!(
                "document symbols returned {count} items; expected at least {min_count}: {value}"
            ),
        ));
    }
    if let Some(expected_name) = expected_name
        && !contains_symbol_name(&value, expected_name)
    {
        return Err(WorkloadError::new(
            FailureKind::Incorrect,
            format!("document symbols did not contain `{expected_name}`: {value}"),
        ));
    }
    Ok(())
}

fn document_symbol_count(value: &Value) -> usize {
    value.as_array().map_or(0, |items| {
        items.iter().map(|item| 1 + item.get("children").map_or(0, document_symbol_count)).sum()
    })
}

fn contains_symbol_name(value: &Value, expected: &str) -> bool {
    value.as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item.get("name").and_then(Value::as_str) == Some(expected)
                || item
                    .get("children")
                    .is_some_and(|children| contains_symbol_name(children, expected))
        })
    })
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Position;
    use serde_json::json;

    #[test]
    fn definition_validator_requires_uri_and_target_range() {
        let expected_uri = Url::parse("file:///tmp/Math.sol").unwrap();
        let expected = Anchor {
            path: PathBuf::from("Math.sol"),
            position: Position { line: 2, character: 10 },
        };
        let value = json!({"uri":"file:///tmp/Math.sol", "range":{"start":{"line":2,"character":5},"end":{"line":2,"character":16}}});
        assert!(validate_definition(value, &expected_uri, &expected).is_ok());
    }

    #[test]
    fn count_and_document_symbol_predicates_reject_incorrect_results() {
        assert!(validate_min_count(json!([1, 2]), 2, "references").is_ok());
        assert!(validate_min_count(json!([1]), 2, "references").is_err());
        let symbols = json!([{
            "name":"Main",
            "children":[
                {"name":"stored"},
                {"name":"calculate","children":[{"name":"input"}]},
                {"name":"status"}
            ]
        }]);
        assert!(validate_document_symbols(symbols.clone(), 5, Some("input")).is_ok());
        assert!(validate_document_symbols(symbols.clone(), 6, None).is_err());
        assert!(validate_document_symbols(symbols, 1, Some("missing")).is_err());
    }

    #[test]
    fn workspace_text_edits_apply_in_reverse_and_use_negotiated_encoding() {
        let mut text = "a😀bc".to_owned();
        let edits = json!([
            {"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":5}},"newText":"X"},
            {"range":{"start":{"line":0,"character":6},"end":{"line":0,"character":7}},"newText":"Y"}
        ]);
        apply_text_edits(&mut text, edits.as_array().unwrap(), PositionEncoding::Utf8).unwrap();
        assert_eq!(text, "aXbY");
    }

    #[test]
    fn closed_server_transport_is_a_crash() {
        let error = classify_request_error(anyhow!("LSP stdout closed unexpectedly"));
        assert!(matches!(error.kind, FailureKind::Crashed));
    }
}
