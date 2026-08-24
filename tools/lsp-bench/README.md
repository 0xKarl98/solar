# Cross-server Solidity LSP benchmark

`solar-lsp-bench` drives the same LSP workloads against a locked inventory of
Solidity language servers. Every response is validated before a sample enters
latency or resource aggregates, so an unsupported or incorrect response cannot
look fast by doing less work.

The v1 inventory is the **core4** set: Solar, Asyncswap, Nomic Foundation, and
official `solc --lsp`. Every server uses stdio. `servers.lock.yaml` records
the selected versions, source revisions, installation commands, and artifact
digests for the external servers. Solar intentionally points at the workspace
binary `../../target/release/solar`; it has no historical source or install
pin in this lock. The runner supplies the exact Solar binary, source revision,
and executable digest as runtime provenance.

`fixtures.lock.yaml` preserves the historical synthetic fixture and the
pinned Uniswap v4-core, Aave v3.7.0 origin, and Optimism contracts-bedrock
corpora, including their compiler and dependency locks.

## Methodology

The workload design is informed by Asyncswap's
[`lsp-bench` at commit `ca0651f86f430290dacdbeb62c9c6987a3ad6966`](https://github.com/asyncswap/lsp-bench/tree/ca0651f86f430290dacdbeb62c9c6987a3ad6966),
especially its sequential warmup and sampling model. This harness also keeps
isolated fixture copies and caches, deterministic server rotation, durable
per-sample correctness, and process accounting.

The workloads cover cold initialization and readiness, warm hover, definition,
references, completion, and document-symbol requests, incremental edit/save,
symbol rename, file create/rename/delete notifications, and fresh, reused, and
invalidated caches. Unsupported operations are recorded separately and excluded
from performance statistics.

## Requirements

Run commands from the repository root. Preparation requires Git, curl, tar,
Rust and Cargo, Node.js, and npm. Downloads, installed servers, compiler
artifacts, and generated reports stay below `target/lsp-bench/`.

The checked-in external artifacts target x86_64 Linux. The npm server uses its
checked-in lockfile-v3 manifest and `npm ci`. Network-isolated execution also
requires `unshare` from util-linux and `ip` from iproute2.

## CLI

Build the harness, fetch the locked inputs, and inspect the audit table:

```bash
cargo build --locked -p solar-lsp-bench
target/debug/solar-lsp-bench prepare
target/debug/solar-lsp-bench doctor
```

`prepare` is the only phase expected to access the network. It checks out
external fixture and server revisions, downloads checksum-pinned compiler
artifacts, and installs declared server versions. It accepts repeatable
`--server ID` and `--fixture ID` filters. `doctor` accepts the same server
filter and checks executable versions, artifacts, source checkouts, fixtures,
and the host accounting capabilities.

The profiles in `benchmark.yaml` are:

| Profile | Warmup | Samples | Cold | Lifecycle | Scope |
| --- | ---: | ---: | ---: | ---: | --- |
| `smoke` | 1 | 2 | 1 | 1 | local synthetic subset |
| `pr-smoke` | 5 | 20 | 4 | 4 | synthetic scenarios |
| `full` | 10 | 100 | 8 | 8 | all scenarios for all core4 fixtures |

The CLI defaults `run` to `pr-smoke`; pass `--profile full` for the complete
core4 matrix.

Run a local smoke check, the portable PR signal, or the full core4 matrix:

```bash
target/debug/solar-lsp-bench run \
  --profile smoke \
  --output target/lsp-bench/smoke

target/debug/solar-lsp-bench run \
  --profile pr-smoke \
  --server solar --server asyncswap --server nomic-foundation --server solc \
  --output target/lsp-bench/pr-smoke

target/debug/solar-lsp-bench run \
  --profile full \
  --output target/lsp-bench/full
```

For a workflow-built Solar executable, add the paired runtime provenance
options `--solar-binary PATH --solar-revision SHA`. They override the manifest
command for that run and record the executable digest and source revision in
the summary.

`--server ID` and `--workload ID` are repeatable filters. `--repeat N`
overrides the profile's independent process-run counts and
`--timeout-secs N` overrides its operation and shutdown timeout. These
options change the benchmark protocol and should be disclosed with resulting
reports.

Regenerate Markdown from a summary, or validate a complete result matrix:

```bash
target/debug/solar-lsp-bench report \
  --input target/lsp-bench/full/summary.json \
  --output target/lsp-bench/full/COMPARISON.md

target/debug/solar-lsp-bench validate-results \
  --profile full \
  --input target/lsp-bench/full
```

`compare` compares two compatible summaries and writes Markdown plus JSON:

```bash
target/debug/solar-lsp-bench compare \
  --baseline target/lsp-bench/baseline/summary.json \
  --candidate target/lsp-bench/full/summary.json \
  --output target/lsp-bench/comparison.md \
  --json-output target/lsp-bench/comparison.json
```

## CI

`.github/workflows/lsp-bench.yml` runs `pr-smoke` against core4 and the
synthetic fixture for pull requests. The job and runner both tolerate failures
so the comparison remains reference-only; reports and raw evidence are still
uploaded. It neither comments on pull requests nor changes the existing Solar
base/candidate benchmark and verdict workflow.

The same workflow exposes a fixed `full` comparison through manual dispatch on
`main`. That job is strict: correctness, timeout, or process failures fail the
job after the available reports and raw evidence have been published.

## Accounting

Every run is portable by default. Portable results are useful for functional
and relative-performance comparisons, but they must not be presented as
authoritative hardware measurements.

Process reports include wall time, CPU, and peak memory. Linux cgroup v2
process-tree metrics are the authoritative backend for session and per-request
resource accounting; fallback process measurements remain visible but are not
authoritative. The summary sets `environment.authoritative: true` only when
the run is on x86_64 Linux, every successful process and request has complete
cgroup process-tree evidence, and every measured process ran in an isolated
network namespace. Otherwise it records `false` and retains the observed
accounting backends.

Use `doctor --publish` to audit whether the host can provide the strict
platform, cgroup, namespace, and clean-worktree prerequisites. There is no
`publish` benchmark profile in v1; an operator can request
`validate-results --require-authoritative` or
`report --require-authoritative` when a result must satisfy those gates.

## Results and provenance

`run` atomically writes `samples.json`, `samples.jsonl`,
`summary.json`, and `summary.md` below the selected output directory.
Raw samples retain failures and outliers for audit; aggregates contain only
samples with `pass` status.

Summaries record hashes of the benchmark and lock manifests, harness and Git
state, observed server versions and executable/artifact hashes, Solar runtime
source provenance, fixture content hashes, compiler and dependency metadata,
Node.js and npm versions for npm-backed servers, platform details, accounting
backends, and the authoritative flag. The npm dependency closure is fixed by
the checked-in package lock and its integrity hashes. The source-built Solar
executable has no portable expected digest; its actual digest is recorded for
the run.
