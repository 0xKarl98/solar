# solar-lsp-bench

`solar-lsp-bench` is a process-isolated, end-to-end performance harness for the Solar LSP. It
drives two pre-built `solar` executables through LSP over stdio and runs identical workloads against
fresh generated Foundry workspaces.

Build the baseline and candidate with the same profile, features, target, and allocator. For
example:

```bash
cargo build --profile profiling --bin solar

cargo run -p solar-lsp-bench -- \
  compare \
  --baseline /tmp/solar-main/target/profiling/solar \
  --candidate target/profiling/solar \
  --scenario all \
  --repeat 10
```

Every `(binary, scenario, repetition)` combination gets a new LSP process and a new 184-file
workspace. Baseline and candidate runs alternate order between repetitions to reduce ordering bias.
External flychecks are disabled.

## Scenarios

- `startup`: process start, LSP initialization, document open, and first matching diagnostics.
- `slow-typing`: identifier edits spaced 110 ms apart.
- `fast-typing`: identifier edits spaced 2 ms apart.
- `file-navigation`: open a fixed sequence of Solidity files and return to the initial file.
- `cross-file-edits`: edit four fixed files with 4 ms between changes, then save them.
- `requests-during-edit`: issue completion, definition, references, and inlay-hint requests while
  editing.
- `watched-files`: create, change, and delete a Solidity file on disk.

`--scenario all` runs every scenario independently. Use `--help` to list all options.

## Results

The output directory defaults to `target/lsp-bench/latest` and contains:

- `samples.json`: every run, LSP event trace, request measurement, process resource measurement,
  and failure.
- `summary.json`: environment, binary versions, benchmark configuration, and aggregate statistics.
- `summary.md`: compact comparison table.

Scenario wall time and latest matching diagnostics latency are reported independently. Diagnostics
publication counts cover the fixed `Treasury.sol` sentinel URI, providing a stable proxy for
completed analysis generations while all protocol messages remain available in the event trace. On
Unix, child CPU time and peak resident memory come from `wait4`, so the default comparison does not
need an external sampler that could disturb the workload. Failed and timed-out runs remain in the
raw samples and are excluded from latency aggregates.
