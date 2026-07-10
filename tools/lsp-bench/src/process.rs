use crate::protocol;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    io::{BufReader, Read},
    path::Path,
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

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
    pub(crate) diagnostic_publications: usize,
    pub(crate) requests: Vec<RequestMeasurement>,
    pub(crate) events: Vec<TraceEvent>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ProcessMetrics {
    pub(crate) wall_ms: f64,
    pub(crate) user_cpu_ms: Option<f64>,
    pub(crate) system_cpu_ms: Option<f64>,
    pub(crate) peak_rss_mib: Option<f64>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stderr: String,
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
    observed_diagnostic_uri: Option<String>,
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
            observed_diagnostic_uri: None,
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

    pub(crate) fn wait_for_diagnostics(&mut self, marker: &str) -> Result<f64> {
        let started_at = Instant::now();
        let deadline = Instant::now() + self.timeout;
        loop {
            let message = self.receive(deadline)?;
            if message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics")
            {
                self.record_diagnostics(&message);
                let contains_marker = message
                    .pointer("/params/diagnostics")
                    .and_then(Value::as_array)
                    .is_some_and(|diagnostics| {
                        diagnostics.iter().any(|diagnostic| {
                            diagnostic
                                .get("message")
                                .and_then(Value::as_str)
                                .is_some_and(|message| message.contains(marker))
                        })
                    });
                if contains_marker {
                    return Ok(duration_ms(started_at.elapsed()));
                }
            } else {
                self.handle_unsolicited(message)?;
            }
        }
    }

    pub(crate) fn clear_observations(&mut self) {
        self.observations = Observations::default();
    }

    pub(crate) fn observe_diagnostics_for(&mut self, uri: impl Into<String>) {
        self.observed_diagnostic_uri = Some(uri.into());
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
        if message.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
        {
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
        let uri = message.pointer("/params/uri").and_then(Value::as_str);
        if self.observed_diagnostic_uri.as_deref().is_none_or(|observed| uri == Some(observed)) {
            self.observations.diagnostic_publications += 1;
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

    pub(crate) fn finish(mut self, graceful: bool) -> Result<ProcessMetrics> {
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

        let wall_ms = duration_ms(self.started_at.elapsed());
        let (status, usage) = wait_with_usage(self.child.take().unwrap())?;
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

        Ok(ProcessMetrics {
            wall_ms,
            user_cpu_ms: usage.user_cpu_ms,
            system_cpu_ms: usage.system_cpu_ms,
            peak_rss_mib: usage.peak_rss_mib,
            exit_code: status.code(),
            stderr,
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

#[derive(Default)]
struct ResourceUsage {
    user_cpu_ms: Option<f64>,
    system_cpu_ms: Option<f64>,
    peak_rss_mib: Option<f64>,
}

#[cfg(unix)]
fn wait_with_usage(child: Child) -> Result<(ExitStatus, ResourceUsage)> {
    let mut status = 0;
    // SAFETY: `rusage` is a plain C struct that is valid when zero-initialized.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    loop {
        // SAFETY: `child.id()` is a live child PID and both output pointers are valid.
        let result = unsafe { libc::wait4(child.id() as libc::pid_t, &mut status, 0, &mut usage) };
        if result >= 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error.into());
        }
    }
    drop(child);

    #[cfg(target_os = "macos")]
    let peak_rss_mib = usage.ru_maxrss as f64 / (1024.0 * 1024.0);
    #[cfg(not(target_os = "macos"))]
    let peak_rss_mib = usage.ru_maxrss as f64 / 1024.0;

    Ok((
        ExitStatus::from_raw(status),
        ResourceUsage {
            user_cpu_ms: Some(timeval_ms(usage.ru_utime)),
            system_cpu_ms: Some(timeval_ms(usage.ru_stime)),
            peak_rss_mib: Some(peak_rss_mib),
        },
    ))
}

#[cfg(not(unix))]
fn wait_with_usage(mut child: Child) -> Result<(ExitStatus, ResourceUsage)> {
    child.wait().map(|status| (status, ResourceUsage::default())).map_err(Into::into)
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

    #[test]
    fn diagnostics_marker_matching_is_exact_to_the_message_contents() {
        let message = json!({
            "params": {"diagnostics": [{"message": "identifier `bench_final` not found"}]}
        });
        let diagnostics = message.pointer("/params/diagnostics").unwrap().as_array().unwrap();

        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic["message"].as_str().unwrap().contains("bench_final")
            })
        );
        assert!(
            !diagnostics.iter().any(|diagnostic| {
                diagnostic["message"].as_str().unwrap().contains("bench_other")
            })
        );
    }
}
