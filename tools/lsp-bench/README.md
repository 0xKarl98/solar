# solar-lsp-bench

`solar-lsp-bench` compares two pre-built `solar` executables by driving each through the same
continuous LSP user session over stdio. Every run uses an independent temporary copy of the full
Solady project at revision `ab96a830e705de13e0f58cfaefadab4ac8257655`; the source checkout is
validated as clean and is never modified.

Build the baseline and candidate with the same profile and features. The benchmark also requires
an absolute path to a fixed Forge build that supports `forge lint --json`. A successful run also
requires its default lint set to emit `mixed-case-variable` as parser-compatible JSON:

```bash
cargo build --profile profiling --bin solar

cargo run -p solar-lsp-bench -- \
  compare \
  --baseline /tmp/solar-main/target/profiling/solar \
  --candidate target/profiling/solar \
  --project /absolute/path/to/solady \
  --forge /absolute/path/to/forge \
  --repeat 10
```

Baseline and candidate order alternates AB/BA between repetitions. The benchmark verifies matching
Solar build settings, the pinned Solady revision and source shape, a clean project working tree,
the Forge version, and `forge lint --help` before starting any measured session.

## User session

One LSP process performs the complete sequence without restarting between phases:

- Open `src/tokens/ERC4626.sol` and wait for a Solar diagnostic barrier.
- Type three distinct 8-character identifiers at 400, 240, and 120 ms per character.
- Request completion and definition at the real `FixedPointMathLib.fullMulDiv` call.
- Open `src/utils/FixedPointMathLib.sol`, verify inlay hints, and make an 8-character cross-file
  edit at 120 ms per character.
- Return to the already open ERC4626 document and verify references include the declaration and
  known call site.
- Remove all benchmark-only markers, rename one local `supply` variable and its use to
  `total_supply`, write the current VFS documents to disk, and send `didSave`.
- Wait for the `forge-lint` / `mixed-case-variable` diagnostic before shutdown.

Initialization passes the selected `forgePath` and leaves `flychecks` unset, exercising the LSP's
default Foundry `forge lint --json` lifecycle. The harness never invokes solc, starts another Solar
compiler, clones a project, accesses the network, or generates Solidity source.

## Results

The output directory defaults to `target/lsp-bench/latest` and contains `samples.json`,
`summary.json`, and `summary.md` using schema version 2. Raw samples preserve protocol events,
request measurements, process exit status, stderr, and failures.

The main comparison reports:

- Last `didChange` write to matching Solar diagnostics for slow, normal, fast, and cross-file edits.
- Completion, definition, inlay-hint, and references request latency after validating non-empty,
  expected results.
- Per-phase Solar analysis triggers, Solar diagnostic publications on the sentinel URI, and the
  checked `unpublished_analysis_proxy` difference.
- `didSave` to the expected Forge diagnostic as a separate end-to-end lifecycle measurement.

The Forge measurement includes external Forge execution and is not a Solar recomputation metric.
`solar_analysis_triggers` counts the initial `didOpen` and content-changing `didChange`
notifications in each phase; these currently map one-to-one to `GlobalState::recompute`. If the
server adds debounce, this becomes a client workload count instead. Solar diagnostic publications
only count the sentinel URI when at least one diagnostic has `source=solar`.
`unpublished_analysis_proxy` is the checked per-run difference between those counts, not an exact
count of analyses cancelled after starting. Cleanup changes are excluded from activity metrics.

Analysis activity is behavioral context rather than a uniform lower-is-better score. Fixed typing
intervals, idle waits, process wall time, CPU time, and peak RSS are intentionally not aggregated.
Failed runs remain in raw samples and do not enter aggregate statistics.
