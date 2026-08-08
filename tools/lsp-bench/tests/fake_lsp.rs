#![allow(unused_crate_dependencies)]

use serde_json::Value;
use std::{fs, net::TcpListener, path::Path, process::Command};

fn read_json(path: &Path) -> Value {
    serde_json::from_reader(fs::File::open(path).unwrap()).unwrap()
}

fn assert_hex_digest(value: &Value, digits: usize) {
    let digest = value.as_str().unwrap();
    assert_eq!(digest.len(), digits, "{digest}");
    assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()), "{digest}");
}

#[test]
fn dispatcher_preserves_out_of_order_messages_and_server_requests() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("fixture");
    fs::create_dir(&fixture).unwrap();
    fs::write(
        fixture.join("Main.sol"),
        "pragma solidity ^0.8.30; contract Main { function call() external pure returns (uint) { return 1; } }\n",
    )
    .unwrap();
    let config = directory.path().join("benchmark.yaml");
    fs::write(
        &config,
        format!(
            r#"version: 1
profiles:
  smoke:
    warmup: 0
    samples: 1
    cold_samples: 1
    lifecycle_samples: 1
    timeout_ms: 2000
servers:
  - id: fake
    command: "{}"
    version_args: [--version]
    env:
      LSP_BENCH_EXPECT_TOOLCHAIN: "1"
    configuration:
      solidity:
        compiler: fake
fixtures:
  - id: synthetic
    root: "{}"
    source_roots: [.]
    solc:
      version: fake
      native: "{}"
    anchors:
      call:
        path: Main.sol
        needle: call
      main:
        path: Main.sol
        needle: contract Main
        offset: 9
scenarios:
  - id: smoke
    fixture: synthetic
    profile: smoke
    steps:
      - kind: open
        path: Main.sol
      - kind: probe
        name: cold-ready
        probe:
          kind: hover
          path: Main.sol
          anchor: call
          expected_text: add
      - kind: probe
        name: completion
        probe:
          kind: completion
          path: Main.sol
          anchor: call
          expected_label: add
  - id: cache-reuse
    fixture: synthetic
    profile: smoke
    steps:
      - kind: open
        path: Main.sol
      - kind: probe
        name: cache-populated
        probe:
          kind: hover
          path: Main.sol
          anchor: call
          expected_text: add
      - kind: restart
      - kind: open
        path: Main.sol
      - kind: probe
        name: cold-ready
        probe:
          kind: hover
          path: Main.sol
          anchor: call
          expected_text: cache-reused
  - id: edit-save
    fixture: synthetic
    profile: smoke
    steps:
      - kind: open
        path: Main.sol
      - kind: replace
        path: Main.sol
        anchor: main
        text: contract Renamed
        probe:
          kind: document-symbol
          path: Main.sol
          expected_name: Renamed
      - kind: save
        path: Main.sol
        probe:
          kind: document-symbol
          path: Main.sol
          expected_name: Renamed
  - id: symbol-rename
    fixture: synthetic
    profile: smoke
    steps:
      - kind: open
        path: Main.sol
      - kind: rename
        path: Main.sol
        anchor: main
        new_name: Renamed
        probe:
          kind: document-symbol
          path: Main.sol
          expected_name: Renamed
  - id: file-lifecycle
    fixture: synthetic
    profile: smoke
    steps:
      - kind: open
        path: Main.sol
      - kind: create-file
        path: Scratch.sol
        text: "pragma solidity ^0.8.30; contract Scratch {{}}"
        probe:
          kind: document-symbol
          path: Scratch.sol
          expected_name: Scratch
      - kind: rename-file
        from: Scratch.sol
        to: Renamed.sol
        probe:
          kind: document-symbol
          path: Renamed.sol
          expected_name: Scratch
      - kind: delete-file
        path: Renamed.sol
        probe:
          kind: document-symbol
          path: Main.sol
          expected_name: Main
  - id: cache-recovery
    fixture: synthetic
    profile: smoke
    steps:
      - kind: open
        path: Main.sol
      - kind: probe
        name: cache-populated
        probe:
          kind: hover
          path: Main.sol
          anchor: call
          expected_text: add
      - kind: restart
        invalidate:
          path: Main.sol
          anchor: main
          text: contract Recovered
      - kind: open
        path: Main.sol
      - kind: probe
        name: cold-ready
        probe:
          kind: document-symbol
          path: Main.sol
          expected_name: Recovered
"#,
            env!("CARGO_BIN_EXE_solar-lsp-bench-fake"),
            fixture.display(),
            env!("CARGO_BIN_EXE_solar-lsp-bench-fake"),
        ),
    )
    .unwrap();
    let output = directory.path().join("results");
    let status = Command::new(env!("CARGO_BIN_EXE_solar-lsp-bench"))
        .args(["run", "--config"])
        .arg(&config)
        .args(["--profile", "smoke", "--repeat", "1", "--output"])
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());

    let summary = read_json(&output.join("summary.json"));
    assert!(
        summary["summaries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|group| group["status_counts"]["pass"] == 1)
    );
    assert_eq!(summary["environment"]["network_isolated"], false);
    assert_hex_digest(&summary["harness_git_revision"], 40);
    assert!(summary["harness_git_dirty"].is_boolean());
    assert_hex_digest(&summary["servers"][0]["executable_sha256"], 64);
    assert_hex_digest(&summary["fixtures"][0]["content_sha256"], 64);
    assert_hex_digest(&summary["fixtures"][0]["solc_native_sha256"], 64);
    let samples = read_json(&output.join("samples.json"));
    let smoke = samples["samples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sample| sample["workload"] == "smoke")
        .unwrap();
    assert_eq!(smoke["observations"]["diagnostic_publications"], 1);
    let server_requests = smoke["observations"]["server_requests"].as_array().unwrap();
    for method in [
        "window/workDoneProgress/create",
        "workspace/configuration",
        "client/registerCapability",
        "workspace/applyEdit",
    ] {
        assert!(
            server_requests
                .iter()
                .any(|request| request["method"] == method && request["handled"] == true),
            "server request {method} was not handled"
        );
    }
    assert!(smoke["observations"]["events"].as_array().unwrap().iter().any(|event| {
        event["direction"] == "receive" && event["id"] == 999 && event["method"].is_null()
    }));
    let cache = samples["samples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sample| sample["workload"] == "cache-reuse")
        .unwrap();
    assert_eq!(cache["setup_phases"].as_array().unwrap().len(), 1);
    assert_eq!(cache["status"], "pass");
    assert!(cache["timings_ms"]["cold_ready_ms"].as_f64().unwrap() >= 70.0);
    let edit = samples["samples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sample| sample["workload"] == "edit-save")
        .unwrap();
    assert!(edit["timings_ms"]["edit_to_edit-ready_ms"].is_number());
    assert!(edit["timings_ms"]["save_to_save-ready_ms"].is_number());
    for method in ["textDocument/didChange", "textDocument/didSave"] {
        assert!(
            edit["observations"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["direction"] == "send" && event["method"] == method),
            "missing {method}"
        );
    }
    let rename = samples["samples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sample| sample["workload"] == "symbol-rename")
        .unwrap();
    assert_eq!(rename["status"], "pass");
    assert!(rename["timings_ms"]["rename_to_rename-ready_ms"].is_number());
    let lifecycle = samples["samples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sample| sample["workload"] == "file-lifecycle")
        .unwrap();
    assert_eq!(lifecycle["status"], "pass");
    for timing in [
        "create-file_to_create-file-ready_ms",
        "rename-file_to_rename-file-ready_ms",
        "delete-file_to_delete-file-ready_ms",
    ] {
        assert!(lifecycle["timings_ms"][timing].is_number(), "missing {timing}");
    }
    let sent_methods = lifecycle["observations"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["direction"] == "send")
        .filter_map(|event| event["method"].as_str())
        .collect::<Vec<_>>();
    for method in
        ["workspace/didCreateFiles", "workspace/didRenameFiles", "workspace/didDeleteFiles"]
    {
        assert!(sent_methods.contains(&method), "missing {method}");
    }
    let recovery = samples["samples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|sample| sample["workload"] == "cache-recovery")
        .unwrap();
    assert_eq!(recovery["setup_phases"].as_array().unwrap().len(), 1);
    assert_eq!(recovery["status"], "pass");
    assert!(recovery["timings_ms"]["cold_ready_ms"].is_number());
    assert!(
        samples["samples"]
            .as_array()
            .unwrap()
            .iter()
            .all(|sample| sample["process"]["network_isolated"] == false)
    );
}

#[test]
fn probe_failures_distinguish_incorrect_results_from_timeouts() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("fixture");
    fs::create_dir(&fixture).unwrap();
    fs::write(
        fixture.join("Main.sol"),
        "pragma solidity ^0.8.30; contract Main { function call() external {} }\n",
    )
    .unwrap();
    let config = directory.path().join("benchmark.yaml");
    fs::write(
        &config,
        format!(
            r#"version: 1
profiles:
  failure:
    warmup: 0
    samples: 1
    cold_samples: 1
    lifecycle_samples: 1
    timeout_ms: 500
    readiness_quiet_ms: 20
servers:
  - id: incorrect
    command: "{}"
    version_args: [--version]
    env:
      LSP_BENCH_FAKE_BEHAVIOR: incorrect-hover
  - id: timeout
    command: "{}"
    version_args: [--version]
    env:
      LSP_BENCH_FAKE_BEHAVIOR: timeout-hover
fixtures:
  - id: synthetic
    root: "{}"
    source_roots: [.]
    solc:
      version: fake
      native: "{}"
    anchors:
      call:
        path: Main.sol
        needle: call
scenarios:
  - id: failing-hover
    fixture: synthetic
    steps:
      - kind: open
        path: Main.sol
      - kind: probe
        name: cold-ready
        probe:
          kind: hover
          path: Main.sol
          anchor: call
          expected_text: add
"#,
            env!("CARGO_BIN_EXE_solar-lsp-bench-fake"),
            env!("CARGO_BIN_EXE_solar-lsp-bench-fake"),
            fixture.display(),
            env!("CARGO_BIN_EXE_solar-lsp-bench-fake"),
        ),
    )
    .unwrap();
    let output = directory.path().join("results");
    let status = Command::new(env!("CARGO_BIN_EXE_solar-lsp-bench"))
        .args(["run", "--config"])
        .arg(&config)
        .args(["--profile", "failure", "--repeat", "1", "--allow-failures", "--output"])
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());

    let samples = read_json(&output.join("samples.json"));
    let samples = samples["samples"].as_array().unwrap();
    let incorrect = samples.iter().find(|sample| sample["server"] == "incorrect").unwrap();
    assert_eq!(incorrect["status"], "incorrect");
    assert_eq!(incorrect["correctness"][0]["ok"], false);
    assert!(incorrect["error"].as_str().unwrap().contains("hover did not contain"));

    let timeout = samples.iter().find(|sample| sample["server"] == "timeout").unwrap();
    assert_eq!(timeout["status"], "timeout");
    assert_eq!(timeout["correctness"][0]["ok"], false);
    assert!(timeout["error"].as_str().unwrap().contains("timed out waiting for LSP message"));
}

#[test]
fn executable_with_a_mismatched_version_is_incompatible() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("fixture");
    fs::create_dir(&fixture).unwrap();
    fs::write(fixture.join("Main.sol"), "contract Main {}\n").unwrap();
    let config = directory.path().join("benchmark.yaml");
    fs::write(
        &config,
        format!(
            r#"version: 1
profiles:
  smoke:
    warmup: 0
    samples: 1
    cold_samples: 1
    lifecycle_samples: 1
    timeout_ms: 500
servers:
  - id: incompatible
    command: "{}"
    version_args: [--version]
    locked_version: "2"
fixtures:
  - id: synthetic
    root: "{}"
    source_roots: [.]
scenarios:
  - id: open
    fixture: synthetic
    steps:
      - kind: open
        path: Main.sol
"#,
            env!("CARGO_BIN_EXE_solar-lsp-bench-fake"),
            fixture.display(),
        ),
    )
    .unwrap();
    let output = directory.path().join("results");
    let status = Command::new(env!("CARGO_BIN_EXE_solar-lsp-bench"))
        .args(["run", "--config"])
        .arg(&config)
        .args(["--profile", "smoke", "--repeat", "1", "--allow-failures", "--output"])
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());

    let summary = read_json(&output.join("summary.json"));
    assert_eq!(summary["servers"][0]["status"], "incompatible");
    assert_eq!(summary["summaries"][0]["status"], "failed");
    let samples = read_json(&output.join("samples.json"));
    assert_eq!(samples["samples"][0]["status"], "incompatible");
}

#[test]
fn tcp_transport_connects_and_shutdown_omits_params() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("fixture");
    fs::create_dir(&fixture).unwrap();
    fs::write(fixture.join("Main.sol"), "contract Main {}\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    let config = directory.path().join("benchmark.yaml");
    fs::write(
        &config,
        format!(
            r#"version: 1
profiles:
  smoke:
    warmup: 0
    samples: 1
    cold_samples: 1
    lifecycle_samples: 1
    timeout_ms: 2000
servers:
  - id: tcp
    command: "{}"
    args: [--tcp, "{address}"]
    version_args: [--version]
    transport:
      kind: tcp
      address: "{address}"
    env:
      LSP_BENCH_FAKE_BEHAVIOR: strict-shutdown
fixtures:
  - id: synthetic
    root: "{}"
    source_roots: [.]
scenarios:
  - id: open
    fixture: synthetic
    steps:
      - kind: open
        path: Main.sol
"#,
            env!("CARGO_BIN_EXE_solar-lsp-bench-fake"),
            fixture.display(),
        ),
    )
    .unwrap();
    let output = directory.path().join("results");
    let status = Command::new(env!("CARGO_BIN_EXE_solar-lsp-bench"))
        .args(["run", "--config"])
        .arg(&config)
        .args(["--profile", "smoke", "--repeat", "1", "--output"])
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());

    let samples = read_json(&output.join("samples.json"));
    assert_eq!(samples["samples"][0]["status"], "pass");
    let shutdown = samples["samples"][0]["observations"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["direction"] == "send" && event["method"] == "shutdown")
        .unwrap();
    assert!(shutdown["message"].get("params").is_none());
}
