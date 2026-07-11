use crate::protocol;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{BufReader, Read},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const PUBLISH_DIAGNOSTICS: &str = "textDocument/publishDiagnostics";

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
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct RequestMeasurement {
    pub(crate) method: String,
    pub(crate) elapsed_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Observations {
    pub(crate) solar_diagnostic_publications: usize,
    pub(crate) requests: Vec<RequestMeasurement>,
    pub(crate) events: Vec<TraceEvent>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcessMetrics {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stderr: String,
}

pub(crate) struct FinishedProcess {
    pub(crate) metrics: ProcessMetrics,
    pub(crate) observations: Observations,
}

pub(crate) struct LspProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    messages: mpsc::Receiver<Result<Value>>,
    stdout_thread: Option<thread::JoinHandle<()>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: i64,
    timeout: Duration,
    started_at: Instant,
    sentinel_uri: Option<String>,
    observations: Observations,
}

impl LspProcess {
    pub(crate) fn spawn(binary: &Path, cwd: &Path, timeout: Duration) -> Result<Self> {
        let started_at = Instant::now();
        let mut child = Command::new(binary)
            .arg("lsp")
            .current_dir(cwd)
            .env_remove("RUST_LOG")
            .env_remove("SOLAR_PROFILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start `{}`", binary.display()))?;
        let stdin = child.stdin.take().context("LSP stdin is unavailable")?;
        let stdout = child.stdout.take().context("LSP stdout is unavailable")?;
        let stderr_pipe = child.stderr.take().context("LSP stderr is unavailable")?;

        let (sender, messages) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match protocol::read_message(&mut reader) {
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

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_buffer = stderr.clone();
        let stderr_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let _ = reader.read_to_end(&mut stderr_buffer.lock().unwrap());
        });

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            messages,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr,
            next_id: 1,
            timeout,
            started_at,
            sentinel_uri: None,
            observations: Observations::default(),
        })
    }

    pub(crate) fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    pub(crate) fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_inner(method, params, true)
    }

    pub(crate) fn setup_request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_inner(method, params, false)
    }

    fn request_inner(&mut self, method: &str, params: Value, measured: bool) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let started_at = Instant::now();
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + self.timeout;

        loop {
            let message = self.receive(deadline)?;
            if message.get("id").and_then(Value::as_i64) == Some(id)
                && message.get("method").is_none()
            {
                if let Some(error) = message.get("error") {
                    bail!("LSP request `{method}` failed: {error}")
                }
                let elapsed_ms = duration_ms(started_at.elapsed());
                if measured {
                    self.observations
                        .requests
                        .push(RequestMeasurement { method: method.to_string(), elapsed_ms });
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.handle_unsolicited(message)?;
        }
    }

    pub(crate) fn wait_for_solar_marker(&mut self, uri: &str, marker: &str) -> Result<f64> {
        self.wait_for_diagnostics(|message| solar_marker_matches(message, uri, marker))
    }

    pub(crate) fn wait_for_no_solar_diagnostics(&mut self, uri: &str) -> Result<f64> {
        self.wait_for_diagnostics(|message| no_solar_diagnostics_match(message, uri))
    }

    pub(crate) fn wait_for_diagnostic(
        &mut self,
        uri: &str,
        source: &str,
        code: &str,
    ) -> Result<f64> {
        self.wait_for_diagnostics(|message| diagnostic_matches(message, uri, source, code))
    }

    fn wait_for_diagnostics(&mut self, matches: impl Fn(&Value) -> bool) -> Result<f64> {
        let started_at = Instant::now();
        let deadline = Instant::now() + self.timeout;
        loop {
            let message = self.receive(deadline)?;
            if message.get("method").and_then(Value::as_str) == Some(PUBLISH_DIAGNOSTICS) {
                self.record_diagnostics(&message);
                if matches(&message) {
                    return Ok(duration_ms(started_at.elapsed()));
                }
            } else {
                self.handle_unsolicited(message)?;
            }
        }
    }

    pub(crate) fn clear_measurements(&mut self) {
        self.observations.solar_diagnostic_publications = 0;
        self.observations.requests.clear();
    }

    pub(crate) fn observe_solar_diagnostics_for(&mut self, uri: impl Into<String>) {
        self.sentinel_uri = Some(uri.into());
    }

    pub(crate) fn solar_diagnostic_publications(&self) -> usize {
        self.observations.solar_diagnostic_publications
    }

    pub(crate) fn observations(&self) -> &Observations {
        &self.observations
    }

    fn send(&mut self, message: Value) -> Result<()> {
        self.record_event(Direction::Send, &message);
        let stdin = self.stdin.as_mut().context("LSP stdin is closed")?;
        protocol::write_message(stdin, &message)
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

    fn handle_unsolicited(&mut self, message: Value) -> Result<()> {
        if message.get("method").and_then(Value::as_str) == Some(PUBLISH_DIAGNOSTICS) {
            self.record_diagnostics(&message);
        }

        if message.get("method").is_some()
            && let Some(id) = message.get("id").cloned()
        {
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": null}))?;
        }
        Ok(())
    }

    fn record_diagnostics(&mut self, message: &Value) {
        if self.sentinel_uri.as_deref().is_some_and(|uri| message_uri(message) == Some(uri))
            && diagnostics(message).is_some_and(|diagnostics| {
                diagnostics.iter().any(|diagnostic| diagnostic_source(diagnostic) == Some("solar"))
            })
        {
            self.observations.solar_diagnostic_publications += 1;
        }
    }

    fn record_event(&mut self, direction: Direction, message: &Value) {
        self.observations.events.push(TraceEvent {
            elapsed_ms: duration_ms(self.started_at.elapsed()),
            direction,
            method: message.get("method").and_then(Value::as_str).map(str::to_owned),
            id: message.get("id").cloned(),
        });
    }

    pub(crate) fn finish(mut self, graceful: bool) -> Result<FinishedProcess> {
        let mut shutdown_error = None;
        if graceful {
            if let Err(error) = self.setup_request("shutdown", Value::Null) {
                shutdown_error = Some(error);
            } else if let Err(error) = self.notify("exit", Value::Null) {
                shutdown_error = Some(error);
            }
        }
        if !graceful || shutdown_error.is_some() {
            let _ = self.child.as_mut().unwrap().kill();
        }
        drop(self.stdin.take());

        let status = self.child.as_mut().unwrap().wait()?;
        self.child.take();
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
        let stderr = String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned();
        if let Some(error) = shutdown_error {
            return Err(error.context(format!("failed to stop LSP; stderr: {stderr}")));
        }

        Ok(FinishedProcess {
            metrics: ProcessMetrics { exit_code: status.code(), stderr },
            observations: std::mem::take(&mut self.observations),
        })
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn solar_marker_matches(message: &Value, uri: &str, marker: &str) -> bool {
    message_uri(message) == Some(uri)
        && diagnostics(message).is_some_and(|diagnostics| {
            diagnostics.iter().any(|diagnostic| {
                diagnostic_source(diagnostic) == Some("solar")
                    && diagnostic
                        .get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| message.contains(marker))
            })
        })
}

fn no_solar_diagnostics_match(message: &Value, uri: &str) -> bool {
    message_uri(message) == Some(uri)
        && diagnostics(message).is_some_and(|diagnostics| {
            diagnostics.iter().all(|diagnostic| diagnostic_source(diagnostic) != Some("solar"))
        })
}

fn diagnostic_matches(message: &Value, uri: &str, source: &str, code: &str) -> bool {
    message_uri(message) == Some(uri)
        && diagnostics(message).is_some_and(|diagnostics| {
            diagnostics.iter().any(|diagnostic| {
                diagnostic_source(diagnostic) == Some(source)
                    && diagnostic.get("code").and_then(Value::as_str) == Some(code)
            })
        })
}

fn message_uri(message: &Value) -> Option<&str> {
    message.pointer("/params/uri").and_then(Value::as_str)
}

fn diagnostics(message: &Value) -> Option<&Vec<Value>> {
    message.pointer("/params/diagnostics").and_then(Value::as_array)
}

fn diagnostic_source(diagnostic: &Value) -> Option<&str> {
    diagnostic.get("source").and_then(Value::as_str)
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solar_marker_requires_exact_uri_source_and_message_contents() {
        let message = diagnostics_message(
            "file:///workspace/src/Test.sol",
            json!({"source": "solar", "message": "identifier `bench_final` not found"}),
        );

        assert!(solar_marker_matches(&message, "file:///workspace/src/Test.sol", "bench_final"));
        assert!(!solar_marker_matches(&message, "file:///workspace/src/Other.sol", "bench_final"));
        assert!(!solar_marker_matches(&message, "file:///workspace/src/Test.sol", "bench_other"));
    }

    #[test]
    fn diagnostic_barriers_distinguish_compiler_and_forge_results() {
        let uri = "file:///workspace/src/Test.sol";
        let clean = diagnostics_message(uri, json!({"source": "forge-lint", "code": "other"}));
        let forge = diagnostics_message(
            uri,
            json!({"source": "forge-lint", "code": "mixed-case-variable"}),
        );

        assert!(no_solar_diagnostics_match(&clean, uri));
        assert!(diagnostic_matches(&forge, uri, "forge-lint", "mixed-case-variable"));
        assert!(!diagnostic_matches(&forge, uri, "solar", "mixed-case-variable"));
    }

    #[test]
    fn publications_only_count_solar_diagnostics_on_the_sentinel_uri() {
        let sentinel = "file:///workspace/src/Sentinel.sol";
        let solar = diagnostics_message(sentinel, json!({"source": "solar"}));
        let forge = diagnostics_message(sentinel, json!({"source": "forge-lint"}));
        let other =
            diagnostics_message("file:///workspace/src/Other.sol", json!({"source": "solar"}));

        assert!(message_has_counted_publication(&solar, sentinel));
        assert!(!message_has_counted_publication(&forge, sentinel));
        assert!(!message_has_counted_publication(&other, sentinel));
    }

    fn diagnostics_message(uri: &str, diagnostic: Value) -> Value {
        json!({
            "method": PUBLISH_DIAGNOSTICS,
            "params": {"uri": uri, "diagnostics": [diagnostic]}
        })
    }

    fn message_has_counted_publication(message: &Value, sentinel: &str) -> bool {
        message_uri(message) == Some(sentinel)
            && diagnostics(message)
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic_source(diagnostic) == Some("solar"))
    }
}
