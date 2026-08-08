# Cross-server Solidity LSP benchmark

`solar-lsp-bench` drives multiple Solidity language servers through the same
LSP workloads over stdio or loopback TCP. It validates every response before including a sample in
latency or resource aggregates, so an unsupported or incorrect result cannot
look fast by returning less work.

The checked-in inventory covers Solar, Asyncswap, Nomic Foundation, official
`solc --lsp`, Wake, Juan Blanco, and qiuxiang. `servers.lock.yaml` records the
selected versions, source revisions, installation commands, and available
artifact digests. `fixtures.lock.yaml` records the synthetic fixture and pinned
revisions of Uniswap v4-core, Aave v3.7.0 origin, and Optimism
contracts-bedrock, together with their compiler and dependency provenance.

## Runner requirements

Run commands from the repository root. Preparation requires Git, curl, tar,
Rust and Cargo, Node.js and npm, and Python with pip and `venv`. Downloads,
source checkouts, installed servers, compiler artifacts, and generated reports
stay below `target/lsp-bench/`.

The checked-in server and compiler artifact definitions target x86_64 Linux. A
full published comparison additionally requires:

- one dedicated, fixed-hardware x86_64 Linux runner with no concurrent jobs;
- cgroup v2 delegation that lets the runner create child cgroups and read CPU,
  memory, and lifecycle counters;
- unprivileged user and network namespaces, used to disconnect measured server
  processes from the network; and
- a clean Git worktree.

`doctor --publish` checks the platform, cgroup delegation, network namespace,
worktree, server source origins/revisions, fixtures, artifacts, and executable
versions. It cannot establish
that hardware and the runner image stayed fixed across separate workflow runs;
that remains an operator requirement. Portable runs, including macOS runs, are
useful for harness development and functional smoke testing but are not
publishable performance comparisons.

## Prepare and audit

Build the harness, fetch the pinned inputs, and inspect the audit table:

```bash
cargo build --locked -p solar-lsp-bench
target/debug/solar-lsp-bench prepare
target/debug/solar-lsp-bench doctor
```

`prepare` is the only phase expected to access the network. It checks out exact
fixture and server revisions, initializes fixture submodules, downloads
checksum-pinned compiler artifacts, and installs the declared server versions.
It accepts repeatable `--server ID` and `--fixture ID` filters for debugging.

The ordinary `doctor` command reports `pass`, `unavailable`, `mismatch`, and
`unpinned` checks but does not fail merely because a check is not `pass`. Use
the strict gate before publishing:

```bash
target/debug/solar-lsp-bench doctor --publish
```

Run `doctor` immediately before the benchmark. `run` performs version and
fixture checks needed for execution, but it is not a substitute for the full
artifact and environment audit.

## Run

For a quick functional pass, use the small sampling profile:

```bash
target/debug/solar-lsp-bench run \
  --profile smoke \
  --output target/lsp-bench/smoke
```

The canonical full run uses the `publish` profile after the strict doctor gate:

```bash
target/debug/solar-lsp-bench doctor --publish
target/debug/solar-lsp-bench run \
  --profile publish \
  --output target/lsp-bench/publish
target/debug/solar-lsp-bench report \
  --input target/lsp-bench/publish/summary.json \
  --output target/lsp-bench/publish/COMPARISON.md
```

Do not pass `--allow-failures` for a publication run. The harness writes all
reports before returning an error, so a server that starts but fails a
correctness assertion remains visible and makes CI fail. Unsupported operations
are recorded separately and are excluded from performance statistics.

`--server ID` and `--workload ID` are repeatable filters. `--repeat N` overrides
the profile's independent process-run counts, while `--timeout-secs N` overrides
its operation and shutdown timeout. These are useful for diagnosis but change
the benchmark protocol and should be disclosed with any resulting report.

Each run rotates server order deterministically and executes samples serially.
Every sample receives a temporary fixture copy and isolated application caches,
pinned `solc` and `forge` aliases, and offline package-manager settings. The
`publish` profile also requires network namespace isolation. The harness does
not clear the host page cache.

The workloads cover cold initialization and correctness readiness, warm hover,
definition, references, completion, and document-symbol requests, incremental
edit/save latency, symbol rename, file create/rename/delete notifications, and
fresh, reused, and invalidated caches. Process reports include wall time, CPU,
and peak memory; only cgroup v2 process-tree accounting is authoritative.

## Results

`run` atomically writes these files below the selected output directory:

- `samples.json`: schema-versioned raw samples and correctness details;
- `samples.jsonl`: one raw sample per line for streaming analysis;
- `summary.json`: provenance, environment, status counts, and aggregates; and
- `summary.md`: the generated human-readable comparison.

`report` regenerates Markdown from a schema-compatible `summary.json` and is
used to produce `COMPARISON.md`. Aggregates contain only samples with `pass`
status. Keep the raw samples whenever publishing a summary so failures and
outliers remain auditable.

`summary.json` records hashes of the benchmark and lock manifests, the harness
version and Git state, observed server versions and executable/artifact hashes,
source revisions, fixture content hashes, actual compiler hashes,
compiler/dependency metadata, platform, logical CPU count, accounting backends,
and whether all successful measured and setup processes met the
Linux/cgroup/network-isolation requirements. Generated Markdown includes the
same core run, server, and fixture provenance before the comparison table. Its
`environment.authoritative`
flag does not encode CPU model, kernel image, background load, worktree state,
or continuity of the physical runner.

The lock files also do not yet pin the complete installation closure. In
particular, npm installs have no package lock and the Wake wheel installation
may resolve transitive Python dependencies at preparation time. The pinned
top-level package artifacts and executed binaries are audited, but comparisons
from different preparation dates still require the workflow provenance and
careful review. The publication workflow captures the resolved npm dependency
trees and Python environment, but recording that state does not make it
immutable. A source-built Solar binary likewise has no portable expected digest;
its exact source revision is pinned and the produced binary digest is recorded
for that run.

## CI publication

`.github/workflows/lsp-bench.yml` is a manual-only full publication workflow.
It targets a self-hosted runner with the `linux`, `x64`, and `lsp-bench` labels;
that label must select the dedicated machine described above. The workflow
prepares every declared input, runs `doctor --publish`, executes the full
`publish` profile, regenerates `COMPARISON.md`, and uploads reports, manifests,
the doctor audit, and host/tool provenance even when a correctness check fails.
