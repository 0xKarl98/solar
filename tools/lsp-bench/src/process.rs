//! JSON-RPC transport and process accounting for external LSP servers.

use crate::{
    config::{CompilerSpec, ServerSpec, TransportSpec},
    protocol,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::{BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[cfg(target_os = "linux")]
use std::{
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    sync::atomic::{AtomicU64, Ordering},
};

const PUBLISH_DIAGNOSTICS: &str = "textDocument/publishDiagnostics";
const MAX_SERVER_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Direction {
    Send,
    Receive,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TraceEvent {
    pub(crate) elapsed_ms: f64,
    pub(crate) direction: Direction,
    pub(crate) method: Option<String>,
    pub(crate) id: Option<Value>,
    pub(crate) message: Value,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RequestMeasurement {
    pub(crate) method: String,
    pub(crate) elapsed_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Observations {
    pub(crate) diagnostic_publications: usize,
    pub(crate) requests: Vec<RequestMeasurement>,
    pub(crate) events: Vec<TraceEvent>,
    pub(crate) server_requests: Vec<ServerRequest>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ServerRequest {
    pub(crate) method: String,
    pub(crate) handled: bool,
    pub(crate) error_code: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProcessAccounting {
    CgroupV2ProcessTree,
    RusageDirectChild,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MemoryAccounting {
    /// Peak total cgroup memory, including anonymous, file, and kernel memory.
    CgroupV2Total,
    /// Peak resident set size reported for the direct child only.
    RusageMaxRssDirectChild,
    Unavailable,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcessMetrics {
    pub(crate) wall_ms: f64,
    pub(crate) user_cpu_ms: Option<f64>,
    pub(crate) system_cpu_ms: Option<f64>,
    pub(crate) peak_memory_mib: Option<f64>,
    pub(crate) accounting: ProcessAccounting,
    pub(crate) memory_accounting: MemoryAccounting,
    pub(crate) process_tree: bool,
    pub(crate) network_isolated: bool,
    pub(crate) cgroup_path: Option<PathBuf>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) forced_kill: bool,
    pub(crate) stderr: String,
}

impl ProcessMetrics {
    pub(crate) fn has_authoritative_process_tree_metrics(&self) -> bool {
        self.process_tree
            && self.accounting == ProcessAccounting::CgroupV2ProcessTree
            && self.user_cpu_ms.is_some()
            && self.system_cpu_ms.is_some()
            && self.peak_memory_mib.is_some()
            && self.memory_accounting == MemoryAccounting::CgroupV2Total
            && self.network_isolated
    }

    pub(crate) fn peak_memory_metric(&self) -> Option<(&'static str, f64)> {
        let name = match self.memory_accounting {
            MemoryAccounting::CgroupV2Total => "peak_cgroup_memory_mib",
            MemoryAccounting::RusageMaxRssDirectChild => "peak_direct_child_rss_mib",
            MemoryAccounting::Unavailable => return None,
        };
        self.peak_memory_mib.map(|value| (name, value))
    }
}

pub(crate) struct FinishedProcess {
    pub(crate) metrics: ProcessMetrics,
    pub(crate) observations: Observations,
}

#[derive(Debug, thiserror::Error)]
#[error("LSP request `{method}` failed with code {code:?}: {message}")]
pub(crate) struct RemoteError {
    pub(crate) method: String,
    pub(crate) code: Option<i64>,
    pub(crate) message: String,
}

/// Isolated HOME, XDG directories, and package-manager caches for one logical
/// benchmark sequence.
#[derive(Clone)]
pub(crate) struct ProcessEnvironment {
    root: Arc<tempfile::TempDir>,
    variables: Arc<BTreeMap<OsString, OsString>>,
    network_isolation: bool,
}

impl ProcessEnvironment {
    pub(crate) fn for_toolchains(
        solc: Option<&CompilerSpec>,
        foundry: Option<&CompilerSpec>,
        network_isolation: bool,
    ) -> Result<Self> {
        let root =
            Arc::new(tempfile::tempdir().context("failed to create isolated server environment")?);
        for name in ["home", "cache", "config", "data", "bin"] {
            std::fs::create_dir_all(root.path().join(name))?;
        }
        let bin = root.path().join("bin");
        let mut variables = BTreeMap::from([
            (OsString::from("LSP_BENCH_OFFLINE"), OsString::from("1")),
            (OsString::from("CARGO_NET_OFFLINE"), OsString::from("true")),
            (OsString::from("npm_config_offline"), OsString::from("true")),
            (OsString::from("PIP_NO_INDEX"), OsString::from("1")),
            (OsString::from("UV_OFFLINE"), OsString::from("1")),
            (OsString::from("FOUNDRY_OFFLINE"), OsString::from("true")),
            (OsString::from("HARDHAT_DISABLE_TELEMETRY"), OsString::from("1")),
        ]);
        if let Some(compiler) = solc {
            if let Some(native) = &compiler.native {
                let alias = bin.join(executable_name("solc"));
                link_tool(native, &alias)?;
                variables.insert(OsString::from("SOLC"), alias.as_os_str().to_owned());
                variables.insert(OsString::from("SOLC_PATH"), alias.as_os_str().to_owned());
                variables.insert(OsString::from("FOUNDRY_SOLC"), alias.as_os_str().to_owned());
            }
            if let Some(soljson) = &compiler.soljson {
                if !soljson.is_file() {
                    bail!("pinned soljson `{}` was not prepared", soljson.display())
                }
                variables.insert(OsString::from("SOLJSON_PATH"), soljson.as_os_str().to_owned());
            }
            variables.insert(OsString::from("SOLC_VERSION"), OsString::from(&compiler.version));
        }
        if let Some(toolchain) = foundry {
            if let Some(native) = &toolchain.native {
                let alias = bin.join(executable_name("forge"));
                link_tool(native, &alias)?;
                variables.insert(OsString::from("FORGE_PATH"), alias.as_os_str().to_owned());
            }
            variables.insert(OsString::from("FOUNDRY_VERSION"), OsString::from(&toolchain.version));
        }
        let path = std::env::join_paths(
            std::iter::once(bin).chain(
                std::env::var_os("PATH")
                    .into_iter()
                    .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>()),
            ),
        )?;
        variables.insert(OsString::from("PATH"), path);
        Ok(Self { root, variables: Arc::new(variables), network_isolation })
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn variables(&self) -> &BTreeMap<OsString, OsString> {
        &self.variables
    }
}

fn executable_name(name: &str) -> OsString {
    #[cfg(windows)]
    return OsString::from(format!("{name}.exe"));
    #[cfg(not(windows))]
    OsString::from(name)
}

fn link_tool(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_file() {
        bail!("pinned tool `{}` was not prepared", source.display())
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, destination)?;
    #[cfg(windows)]
    std::fs::copy(source, destination)?;
    Ok(())
}

pub(crate) struct LspProcess {
    child: Option<Child>,
    writer: Option<Box<dyn Write + Send>>,
    messages: mpsc::Receiver<Result<Value>>,
    stdout_thread: Option<thread::JoinHandle<()>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: i64,
    timeout: Duration,
    started_at: Instant,
    root_uri: String,
    initialization_options: Value,
    configuration: Value,
    observations: Observations,
    capabilities: Value,
    text_sync_kind: Option<u8>,
    position_encoding: String,
    process_group: bool,
    pending_responses: BTreeMap<String, Value>,
    dynamic_capabilities: BTreeSet<String>,
    active_progress: BTreeSet<String>,
    workspace_edits: Vec<Value>,
    _environment: ProcessEnvironment,
    network_isolated: bool,
    cgroup: Option<CgroupHandle>,
}

impl LspProcess {
    pub(crate) fn spawn_with_environment(
        spec: &ServerSpec,
        cwd: &Path,
        timeout: Duration,
        environment: ProcessEnvironment,
    ) -> Result<Self> {
        if environment.network_isolation && matches!(spec.transport, TransportSpec::Tcp { .. }) {
            bail!("TCP LSP transport is incompatible with network namespace isolation")
        }
        if let TransportSpec::Tcp { address } = spec.transport {
            TcpListener::bind(address).with_context(|| {
                format!("TCP LSP address `{address}` for server `{}` is already in use", spec.id)
            })?;
        }
        let started_at = Instant::now();
        let home = environment.path().join("home");
        let cache = environment.path().join("cache");
        let config = environment.path().join("config");
        let data = environment.path().join("data");
        #[cfg(target_os = "linux")]
        let (cgroup, cgroup_procs) = match CgroupHandle::create_linux() {
            Ok((cgroup, procs)) => (Some(cgroup), Some(procs)),
            Err(_) => (None, None),
        };
        #[cfg(not(target_os = "linux"))]
        let cgroup = None;
        let mut command = server_command(spec, environment.network_isolation)?;
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        command
            .current_dir(cwd)
            .env_remove("RUST_LOG")
            .env_remove("SOLAR_PROFILE")
            .env("NO_COLOR", "1")
            .env("LANG", "C")
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", &cache)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_DATA_HOME", &data)
            .env("npm_config_cache", cache.join("npm"))
            .env("PIP_CACHE_DIR", cache.join("pip"))
            .envs(environment.variables());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            #[cfg(target_os = "linux")]
            if let Some(cgroup_procs) = cgroup_procs {
                unsafe {
                    command.pre_exec(move || {
                        let bytes = b"0\n";
                        let written = libc::write(
                            cgroup_procs.as_raw_fd(),
                            bytes.as_ptr().cast(),
                            bytes.len(),
                        );
                        if written == bytes.len() as isize {
                            Ok(())
                        } else {
                            Err(std::io::Error::last_os_error())
                        }
                    });
                }
            }
        }
        let stdio_transport = matches!(spec.transport, TransportSpec::Stdio);
        let mut child = command
            .stdin(if stdio_transport { Stdio::piped() } else { Stdio::null() })
            .stdout(if stdio_transport { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start server `{}` with command `{}`",
                    spec.id,
                    display_command(spec)
                )
            })?;
        let stderr_pipe = child.stderr.take().context("LSP stderr is unavailable")?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_buffer = stderr.clone();
        let stderr_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let _ = reader.read_to_end(
                &mut stderr_buffer.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
        });

        let (writer, reader): (Box<dyn Write + Send>, Box<dyn Read + Send>) = match spec.transport {
            TransportSpec::Stdio => (
                Box::new(child.stdin.take().context("LSP stdin is unavailable")?),
                Box::new(child.stdout.take().context("LSP stdout is unavailable")?),
            ),
            TransportSpec::Tcp { address } => {
                let stream = match connect_tcp(&mut child, address, timeout) {
                    Ok(stream) => stream,
                    Err(error) => {
                        terminate_child(child, true)?;
                        let _ = stderr_thread.join();
                        let stderr = String::from_utf8_lossy(
                            &stderr.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                        )
                        .into_owned();
                        return Err(error.context(format!(
                            "failed to connect to TCP LSP server `{}`; stderr: {stderr}",
                            spec.id
                        )));
                    }
                };
                stream.set_nodelay(true)?;
                (Box::new(stream.try_clone()?), Box::new(stream))
            }
        };
        let (sender, messages) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                match protocol::read_message_limited(&mut reader, MAX_SERVER_MESSAGE_BYTES) {
                    Ok(Some(message)) => {
                        if sender.send(Ok(message)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        return;
                    }
                }
            }
        });

        Ok(Self {
            child: Some(child),
            writer: Some(writer),
            messages,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr,
            next_id: 1,
            timeout,
            started_at,
            root_uri: String::new(),
            initialization_options: spec.initialization_options.clone(),
            configuration: spec.configuration.clone(),
            observations: Observations::default(),
            capabilities: Value::Null,
            text_sync_kind: None,
            position_encoding: "utf-16".into(),
            process_group: true,
            pending_responses: BTreeMap::new(),
            dynamic_capabilities: BTreeSet::new(),
            active_progress: BTreeSet::new(),
            workspace_edits: Vec::new(),
            network_isolated: environment.network_isolation,
            _environment: environment,
            cgroup,
        })
    }

    pub(crate) fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    pub(crate) fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_inner(method, Some(params), true)
    }

    pub(crate) fn setup_request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_inner(method, Some(params), false)
    }

    fn setup_request_without_params(&mut self, method: &str) -> Result<Value> {
        self.request_inner(method, None, false)
    }

    pub(crate) fn set_root(&mut self, uri: &str) {
        self.root_uri = uri.to_owned();
    }

    pub(crate) fn set_initialize_result(&mut self, result: &Value) {
        self.capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        self.text_sync_kind = text_sync_kind(&self.capabilities);
        self.position_encoding = self
            .capabilities
            .get("positionEncoding")
            .and_then(Value::as_str)
            .unwrap_or("utf-16")
            .to_owned();
    }

    pub(crate) fn supports(&self, method: &str) -> bool {
        if self.dynamic_capabilities.contains(method) {
            return true;
        }
        let key = match method {
            "textDocument/definition" => "definitionProvider",
            "textDocument/completion" => "completionProvider",
            "textDocument/hover" => "hoverProvider",
            "textDocument/references" => "referencesProvider",
            "textDocument/documentSymbol" => "documentSymbolProvider",
            "textDocument/rename" => "renameProvider",
            _ => return true,
        };
        self.capabilities
            .get(key)
            .is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
    }

    pub(crate) fn incremental_sync(&self) -> bool {
        self.text_sync_kind != Some(1)
    }

    pub(crate) fn position_encoding(&self) -> &str {
        &self.position_encoding
    }

    pub(crate) fn observations(&self) -> &Observations {
        &self.observations
    }

    pub(crate) fn take_workspace_edits(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.workspace_edits)
    }

    pub(crate) fn process_started_at(&self) -> Instant {
        self.started_at
    }

    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn wait_for_readiness(&mut self, quiet: Duration) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        let mut quiet_deadline = Instant::now() + quiet;
        loop {
            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for LSP indexing readiness")
            }
            if self.active_progress.is_empty() && now >= quiet_deadline {
                return Ok(());
            }
            let receive_deadline = if self.active_progress.is_empty() {
                quiet_deadline.min(deadline)
            } else {
                deadline
            };
            let remaining = receive_deadline.saturating_duration_since(now);
            match self.messages.recv_timeout(remaining) {
                Ok(message) => {
                    let message = message?;
                    self.record_event(Direction::Receive, &message);
                    self.dispatch(message)?;
                    quiet_deadline = Instant::now() + quiet;
                }
                Err(mpsc::RecvTimeoutError::Timeout) if self.active_progress.is_empty() => {
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("timed out waiting for LSP indexing progress to finish")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("LSP stdout closed while waiting for indexing readiness")
                }
            }
        }
    }

    pub(crate) fn initialization_options(&self) -> Value {
        self.initialization_options.clone()
    }

    pub(crate) fn request_inner(
        &mut self,
        method: &str,
        params: Option<Value>,
        measured: bool,
    ) -> Result<Value> {
        let id = Value::from(self.next_id);
        self.next_id += 1;
        let key = id_key(&id)?;
        let started_at = Instant::now();
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(message)?;
        let deadline = Instant::now() + self.timeout;

        loop {
            if let Some(message) = self.pending_responses.remove(&key) {
                if let Some(error) = message.get("error") {
                    return Err(RemoteError {
                        method: method.to_owned(),
                        code: error.get("code").and_then(Value::as_i64),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown remote error")
                            .to_owned(),
                    }
                    .into());
                }
                if measured {
                    self.observations.requests.push(RequestMeasurement {
                        method: method.to_owned(),
                        elapsed_ms: duration_ms(started_at.elapsed()),
                    });
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            let message = self.receive(deadline)?;
            self.dispatch(message)?;
        }
    }

    pub(crate) fn send_change(
        &mut self,
        uri: &str,
        version: i32,
        start: Value,
        end: Value,
        replacement: &str,
        full_text: &str,
    ) -> Result<()> {
        let change = if self.incremental_sync() {
            json!({"range": {"start": start, "end": end}, "text": replacement})
        } else {
            json!({"text": full_text})
        };
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [change]
            }),
        )
    }

    pub(crate) fn finish(mut self, graceful: bool) -> Result<FinishedProcess> {
        let mut shutdown_error = None;
        if graceful {
            if let Err(error) = self.setup_request_without_params("shutdown") {
                shutdown_error = Some(error);
            } else if let Err(error) = self.notify("exit", Value::Null) {
                shutdown_error = Some(error);
            }
        }

        self.writer.take();

        let child = self.child.take().context("LSP child is unavailable")?;
        let (status, usage, mut forced_kill) =
            wait_with_usage(child, self.timeout, self.process_group)?;
        if let Some(cgroup) = &self.cgroup {
            forced_kill |= cgroup.kill_and_wait(self.timeout)?;
        }
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        let stderr = String::from_utf8_lossy(
            &self.stderr.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_owned();
        if let Some(error) = shutdown_error {
            return Err(error.context(format!("failed to stop LSP; stderr: {stderr}")));
        }
        let cgroup_path = self.cgroup.as_ref().map(CgroupHandle::path).cloned();
        let cgroup_metrics = self.cgroup.as_ref().and_then(CgroupHandle::read_metrics);
        let (
            user_cpu_ms,
            system_cpu_ms,
            peak_memory_mib,
            accounting,
            memory_accounting,
            process_tree,
        ) = if let Some(metrics) = cgroup_metrics {
            (
                metrics.user_cpu_ms,
                metrics.system_cpu_ms,
                metrics.peak_memory_mib,
                ProcessAccounting::CgroupV2ProcessTree,
                if metrics.peak_memory_mib.is_some() {
                    MemoryAccounting::CgroupV2Total
                } else {
                    MemoryAccounting::Unavailable
                },
                true,
            )
        } else {
            (
                usage.user_cpu_ms,
                usage.system_cpu_ms,
                usage.peak_rss_mib,
                usage.accounting,
                usage.memory_accounting,
                false,
            )
        };
        Ok(FinishedProcess {
            metrics: ProcessMetrics {
                wall_ms: duration_ms(self.started_at.elapsed()),
                user_cpu_ms,
                system_cpu_ms,
                peak_memory_mib,
                accounting,
                memory_accounting,
                process_tree,
                network_isolated: self.network_isolated,
                cgroup_path,
                exit_code: status.code(),
                forced_kill,
                stderr,
            },
            observations: self.observations.clone(),
        })
    }

    fn send(&mut self, message: Value) -> Result<()> {
        self.record_event(Direction::Send, &message);
        let writer = self.writer.as_mut().context("LSP transport is closed")?;
        protocol::write_message(writer, &message)
    }

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for LSP message")
        }
        let message = self.messages.recv_timeout(remaining).map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => anyhow!("timed out waiting for LSP message"),
            mpsc::RecvTimeoutError::Disconnected => anyhow!("LSP stdout closed unexpectedly"),
        })??;
        self.record_event(Direction::Receive, &message);
        Ok(message)
    }

    fn dispatch(&mut self, message: Value) -> Result<()> {
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        let id = message.get("id").cloned();
        match (method, id) {
            (None, Some(id)) => {
                let key = id_key(&id)?;
                if self.pending_responses.insert(key.clone(), message).is_some() {
                    bail!("received duplicate LSP response id `{key}`")
                }
                Ok(())
            }
            (Some(method), Some(id)) => self.handle_server_request(&method, id, &message),
            (Some(method), None) => {
                if method == PUBLISH_DIAGNOSTICS {
                    self.observations.diagnostic_publications += 1;
                }
                if method == "$/progress"
                    && let Some(token) = message.pointer("/params/token")
                    && let Some(kind) =
                        message.pointer("/params/value/kind").and_then(Value::as_str)
                {
                    let token = id_key(token)?;
                    match kind {
                        "begin" => {
                            self.active_progress.insert(token);
                        }
                        "end" => {
                            self.active_progress.remove(&token);
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
            (None, None) => bail!("received invalid JSON-RPC message without `id` or `method`"),
        }
    }

    fn handle_server_request(&mut self, method: &str, id: Value, message: &Value) -> Result<()> {
        let (result, handled) = match method {
            "workspace/configuration" => {
                let items = message
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let values = items
                    .iter()
                    .map(|item| configuration_value(&self.configuration, item))
                    .collect::<Vec<_>>();
                (Value::Array(values), true)
            }
            "workspace/workspaceFolders" => {
                (json!([{"uri": self.root_uri, "name": "lsp-bench"}]), true)
            }
            "client/registerCapability" => {
                if let Some(registrations) =
                    message.pointer("/params/registrations").and_then(Value::as_array)
                {
                    for registration in registrations {
                        if let Some(method) = registration.get("method").and_then(Value::as_str) {
                            self.dynamic_capabilities.insert(method.to_owned());
                        }
                    }
                }
                (Value::Null, true)
            }
            "client/unregisterCapability" => {
                let registrations = message
                    .pointer("/params/unregisterations")
                    .or_else(|| message.pointer("/params/unregistrations"))
                    .and_then(Value::as_array);
                if let Some(registrations) = registrations {
                    for registration in registrations {
                        if let Some(method) = registration.get("method").and_then(Value::as_str) {
                            self.dynamic_capabilities.remove(method);
                        }
                    }
                }
                (Value::Null, true)
            }
            "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "window/showDocument" => (Value::Null, true),
            "workspace/applyEdit" => {
                if let Some(edit) = message.pointer("/params/edit") {
                    self.workspace_edits.push(edit.clone());
                }
                (json!({"applied": true}), true)
            }
            _ => (Value::Null, false),
        };
        self.observations.server_requests.push(ServerRequest {
            method: method.to_owned(),
            handled,
            error_code: (!handled).then_some(-32601),
        });
        if handled {
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
        } else {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            }))
        }
    }

    fn record_event(&mut self, direction: Direction, message: &Value) {
        self.observations.events.push(TraceEvent {
            elapsed_ms: duration_ms(self.started_at.elapsed()),
            direction,
            method: message.get("method").and_then(Value::as_str).map(str::to_owned),
            id: message.get("id").cloned(),
            message: message.clone(),
        });
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = terminate_child(child, self.process_group);
        }
        if let Some(cgroup) = &self.cgroup {
            let _ = cgroup.kill_and_wait(self.timeout);
        }
    }
}

fn server_command(spec: &ServerSpec, network_isolation: bool) -> Result<Command> {
    if !network_isolation {
        let mut command = Command::new(&spec.command);
        command.args(&spec.args);
        return Ok(command);
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("unshare");
        command
            .args(["--user", "--map-root-user", "--net", "--"])
            .arg(&spec.command)
            .args(&spec.args);
        Ok(command)
    }
    #[cfg(not(target_os = "linux"))]
    bail!("network namespace isolation is only available on Linux")
}

fn connect_tcp(
    child: &mut Child,
    address: std::net::SocketAddr,
    timeout: Duration,
) -> Result<TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match TcpStream::connect_timeout(&address, Duration::from_millis(50)) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() >= deadline => {
                return Err(error).with_context(|| format!("timed out connecting to `{address}`"));
            }
            Err(_) => {}
        }
        if let Some(status) = child.try_wait()? {
            bail!("server exited with status {status} before listening on `{address}`")
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn network_isolation_available() -> Result<()> {
    let status = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to execute `unshare`")?;
    if !status.success() {
        bail!("unprivileged user/network namespaces are unavailable")
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn network_isolation_available() -> Result<()> {
    bail!("network namespace isolation is only available on Linux")
}

fn display_command(spec: &ServerSpec) -> String {
    std::iter::once(spec.command.display().to_string())
        .chain(spec.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn id_key(id: &Value) -> Result<String> {
    match id {
        Value::String(value) => Ok(format!("s:{value}")),
        Value::Number(value) => Ok(format!("n:{value}")),
        _ => bail!("invalid JSON-RPC id `{id}`"),
    }
}

fn configuration_value(configuration: &Value, item: &Value) -> Value {
    let Some(section) = item.get("section").and_then(Value::as_str) else {
        return configuration.clone();
    };
    let mut value = configuration;
    for component in section.split('.') {
        let Some(next) = value.get(component) else { return Value::Null };
        value = next;
    }
    value.clone()
}

fn text_sync_kind(capabilities: &Value) -> Option<u8> {
    let value = capabilities.get("textDocumentSync")?;
    value
        .as_u64()
        .map(|value| value as u8)
        .or_else(|| value.get("change").and_then(Value::as_u64).map(|value| value as u8))
}

#[cfg(target_os = "linux")]
static NEXT_CGROUP_ID: AtomicU64 = AtomicU64::new(0);

struct CgroupHandle {
    path: PathBuf,
}

#[derive(Clone, Copy)]
struct CgroupMetrics {
    user_cpu_ms: Option<f64>,
    system_cpu_ms: Option<f64>,
    peak_memory_mib: Option<f64>,
}

impl CgroupMetrics {
    #[cfg(any(target_os = "linux", test))]
    fn is_complete(&self) -> bool {
        self.user_cpu_ms.is_some() && self.system_cpu_ms.is_some() && self.peak_memory_mib.is_some()
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn cgroup_v2_process_tree_available() -> Result<PathBuf> {
    let (handle, procs) = CgroupHandle::create_linux()?;
    let path = handle.path.clone();
    drop(procs);
    drop(handle);
    Ok(path)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cgroup_v2_process_tree_available() -> Result<PathBuf> {
    bail!("cgroup v2 process-tree accounting is only available on Linux")
}

impl CgroupHandle {
    #[cfg(target_os = "linux")]
    fn create_linux() -> Result<(Self, File)> {
        let membership = std::fs::read_to_string("/proc/self/cgroup")?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .context("current process has no cgroup v2 membership")?;
        let parent = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        let id = NEXT_CGROUP_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("solar-lsp-bench-{}-{id}", std::process::id()));
        std::fs::create_dir(&path)?;
        let handle = Self { path };
        let complete_metrics = handle.read_metrics().is_some_and(|metrics| metrics.is_complete());
        if !complete_metrics || !handle.path.join("cgroup.events").is_file() {
            bail!("cgroup v2 process-tree CPU and peak-memory accounting is unavailable")
        }
        let procs = OpenOptions::new().write(true).open(handle.path.join("cgroup.procs"))?;
        Ok((handle, procs))
    }

    fn path(&self) -> &PathBuf {
        &self.path
    }

    #[cfg(target_os = "linux")]
    fn read_metrics(&self) -> Option<CgroupMetrics> {
        let cpu = std::fs::read_to_string(self.path.join("cpu.stat")).ok()?;
        let mut user_cpu_ms = None;
        let mut system_cpu_ms = None;
        for line in cpu.lines() {
            let (name, value) = line.split_once(' ')?;
            let value = value.parse::<f64>().ok()? / 1_000.0;
            match name {
                "user_usec" => user_cpu_ms = Some(value),
                "system_usec" => system_cpu_ms = Some(value),
                _ => {}
            }
        }
        let peak_memory_mib = std::fs::read_to_string(self.path.join("memory.peak"))
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .map(|bytes| bytes / (1024.0 * 1024.0));
        Some(CgroupMetrics { user_cpu_ms, system_cpu_ms, peak_memory_mib })
    }

    #[cfg(target_os = "linux")]
    fn kill_and_wait(&self, timeout: Duration) -> Result<bool> {
        if !self.is_populated()? {
            return Ok(false);
        }
        let kill = self.path.join("cgroup.kill");
        if kill.is_file() {
            std::fs::write(kill, "1\n")?;
        } else {
            self.kill_members()?;
        }
        let deadline = Instant::now() + timeout;
        loop {
            if !self.is_populated()? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for benchmark cgroup to become empty")
            }
            if !kill.is_file() {
                self.kill_members()?;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(target_os = "linux")]
    fn is_populated(&self) -> Result<bool> {
        let events = std::fs::read_to_string(self.path.join("cgroup.events"))?;
        events
            .lines()
            .find_map(|line| line.strip_prefix("populated "))
            .map(|value| value == "1")
            .context("benchmark cgroup has no `populated` event")
    }

    #[cfg(target_os = "linux")]
    fn kill_members(&self) -> Result<()> {
        for pid in std::fs::read_to_string(self.path.join("cgroup.procs"))?.lines() {
            if let Ok(pid) = pid.parse::<libc::pid_t>() {
                unsafe {
                    let _ = libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn kill_and_wait(&self, _timeout: Duration) -> Result<bool> {
        Ok(false)
    }

    #[cfg(not(target_os = "linux"))]
    fn read_metrics(&self) -> Option<CgroupMetrics> {
        None
    }
}

impl Drop for CgroupHandle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

struct ResourceUsage {
    user_cpu_ms: Option<f64>,
    system_cpu_ms: Option<f64>,
    peak_rss_mib: Option<f64>,
    accounting: ProcessAccounting,
    memory_accounting: MemoryAccounting,
}

#[cfg(unix)]
fn wait_with_usage(
    child: Child,
    timeout: Duration,
    process_group: bool,
) -> Result<(ExitStatus, ResourceUsage, bool)> {
    let pid = child.id() as libc::pid_t;
    let deadline = Instant::now() + timeout;
    let mut status = 0;
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    let mut forced_kill = false;
    loop {
        let result = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut usage) };
        if result == pid {
            forced_kill |= kill_remaining_process_group(pid, process_group);
            drop(child);
            return Ok((ExitStatus::from_raw(status), resource_usage(usage), forced_kill));
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error.into());
            }
        }
        if Instant::now() >= deadline {
            forced_kill = true;
            kill_process_group(pid, process_group);
            loop {
                let result = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
                if result == pid {
                    drop(child);
                    return Ok((ExitStatus::from_raw(status), resource_usage(usage), forced_kill));
                }
                if result < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(not(unix))]
fn wait_with_usage(
    mut child: Child,
    timeout: Duration,
    _process_group: bool,
) -> Result<(ExitStatus, ResourceUsage, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, unavailable_resource_usage(), false));
        }
        if Instant::now() >= deadline {
            child.kill()?;
            return Ok((child.wait()?, unavailable_resource_usage(), true));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn resource_usage(usage: libc::rusage) -> ResourceUsage {
    #[cfg(target_os = "macos")]
    let peak_rss_mib = usage.ru_maxrss as f64 / (1024.0 * 1024.0);
    #[cfg(not(target_os = "macos"))]
    let peak_rss_mib = usage.ru_maxrss as f64 / 1024.0;
    ResourceUsage {
        user_cpu_ms: Some(timeval_ms(usage.ru_utime)),
        system_cpu_ms: Some(timeval_ms(usage.ru_stime)),
        peak_rss_mib: Some(peak_rss_mib),
        accounting: ProcessAccounting::RusageDirectChild,
        memory_accounting: MemoryAccounting::RusageMaxRssDirectChild,
    }
}

#[cfg(not(unix))]
fn unavailable_resource_usage() -> ResourceUsage {
    ResourceUsage {
        user_cpu_ms: None,
        system_cpu_ms: None,
        peak_rss_mib: None,
        accounting: ProcessAccounting::Unavailable,
        memory_accounting: MemoryAccounting::Unavailable,
    }
}

#[cfg(unix)]
fn kill_process_group(pid: libc::pid_t, process_group: bool) {
    if process_group {
        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    unsafe {
        let _ = libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(unix)]
fn kill_remaining_process_group(pid: libc::pid_t, process_group: bool) -> bool {
    process_group && unsafe { libc::kill(-pid, libc::SIGKILL) == 0 }
}

#[cfg(unix)]
fn terminate_child(mut child: Child, process_group: bool) -> Result<()> {
    let pid = child.id() as libc::pid_t;
    kill_process_group(pid, process_group);
    let _ = child.wait();
    Ok(())
}

#[cfg(not(unix))]
fn terminate_child(mut child: Child, _process_group: bool) -> Result<()> {
    child.kill().ok();
    child.wait().ok();
    Ok(())
}

#[cfg(unix)]
fn timeval_ms(value: libc::timeval) -> f64 {
    value.tv_sec as f64 * 1_000.0 + value.tv_usec as f64 / 1_000.0
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerSpec;
    use std::{collections::BTreeMap, fs};

    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    #[test]
    fn text_sync_capability_supports_numeric_and_object_forms() {
        assert_eq!(text_sync_kind(&json!({"textDocumentSync": 1})), Some(1));
        assert_eq!(text_sync_kind(&json!({"textDocumentSync": {"change": 2}})), Some(2));
        assert_eq!(text_sync_kind(&json!({})), None);
    }

    #[test]
    fn command_display_includes_arguments() {
        let spec = ServerSpec {
            id: "server".into(),
            command: "server".into(),
            args: vec!["--stdio".into()],
            transport: TransportSpec::Stdio,
            version_args: vec!["--version".into()],
            locked_version: None,
            expected_version: None,
            enabled: true,
            env: BTreeMap::new(),
            initialization_options: Value::Null,
            configuration: Value::Null,
            label: None,
            source: None,
            install: None,
            artifact: None,
            required: false,
        };
        assert_eq!(display_command(&spec), "server --stdio");
    }

    #[test]
    fn response_ids_preserve_string_and_number_domains() {
        assert_eq!(id_key(&json!(1)).unwrap(), "n:1");
        assert_eq!(id_key(&json!("1")).unwrap(), "s:1");
        assert!(id_key(&Value::Null).is_err());
    }

    #[test]
    fn workspace_configuration_resolves_dotted_sections() {
        let configuration = json!({"solidity": {"compiler": {"version": "0.8.30"}}});
        assert_eq!(
            configuration_value(&configuration, &json!({"section": "solidity.compiler"})),
            json!({"version": "0.8.30"})
        );
        assert_eq!(
            configuration_value(&configuration, &json!({"section": "missing"})),
            Value::Null
        );
    }

    #[test]
    fn cgroup_metrics_require_cpu_breakdown_and_peak_memory() {
        let complete = CgroupMetrics {
            user_cpu_ms: Some(1.0),
            system_cpu_ms: Some(2.0),
            peak_memory_mib: Some(3.0),
        };
        assert!(complete.is_complete());
        assert!(!CgroupMetrics { user_cpu_ms: None, ..complete }.is_complete());
        assert!(!CgroupMetrics { system_cpu_ms: None, ..complete }.is_complete());
        assert!(!CgroupMetrics { peak_memory_mib: None, ..complete }.is_complete());
    }

    #[cfg(unix)]
    #[test]
    fn waiting_for_a_group_leader_kills_remaining_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("descendant.pid");
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & echo $! > \"$1\"", "lsp-bench"])
            .arg(&pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid_file.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let descendant =
            fs::read_to_string(&pid_file).unwrap().trim().parse::<libc::pid_t>().unwrap();

        let (_, _, forced_kill) = wait_with_usage(child, Duration::from_secs(2), true).unwrap();
        assert!(forced_kill, "cleaning up a surviving descendant must be recorded");

        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(descendant) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if process_exists(descendant) {
            unsafe {
                libc::kill(descendant, libc::SIGKILL);
            }
            panic!("descendant process {descendant} survived its process-group leader");
        }
    }

    #[cfg(unix)]
    fn process_exists(pid: libc::pid_t) -> bool {
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}
