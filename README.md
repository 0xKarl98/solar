# Folding-range benchmark evidence

Local macOS results for the folding-range optimization in [commit 2bad5fccd](https://github.com/0xKarl98/solar/commit/2bad5fccdd5c0f23b47af5c7968612592b11fd18). The charts use the retained protocol samples and unchanged Criterion suite. See `measurements.json` for all three protocol workloads, all seven Criterion cases, raw session samples, confidence bounds, and source/binary hashes. The paired binaries were built in the same checkout with matching profiles: release for stdio and bench for Criterion.

The baseline is `fec613fd978a43e3f46bbd36f9095e4b5caafedf`; the candidate adds only `folding.patch`. That baseline includes unrelated signature/selection changes from #1453. The folding module and benchmark source are identical to the PR base `b4727b778bf0c9f9b5113fd88f78f79b18757275`.

## Protocol measurements

Each timed sample sends `didChange` followed by `textDocument/foldingRange` over stdio. A changed leading comment invalidates the folding cache on every sample. All responses must match the baseline exactly. Every session initializes a fresh server, opens the document, waits for initial diagnostics, then pauses for 100 ms outside the timed region. The harness runs three warmups and alternates baseline-first and candidate-first order across eight sessions per build. The chart reports the mean of session medians, with 16 measured samples per session for Optimism and 32 for the smaller sources.

| Source | Before | After | Latency reduction |
|---|---:|---:|---:|
| Optimism | 51.6337 ms | 37.6590 ms | 27.1% |
| Uniswap V3 | 1.1984 ms | 0.9743 ms | 18.7% |
| Unifap router | 0.2628 ms | 0.2591 ms | Inconclusive |

Optimism returns 16,059 folding ranges. Exact paired-bootstrap 95% intervals for its latency reduction are 21.7–30.0% in baseline-first sessions and 26.1–29.4% in candidate-first sessions. Its p95 result is inconclusive in one order stratum. Small router requests have no demonstrated improvement; these measurements do not imply gains for all LSP requests.

The protocol harness is `folding-rpc.py`. With matching baseline and candidate release binaries built from the revisions above, run from the repository root:

```sh
python3 /path/to/folding-rpc.py \
  --base /path/to/baseline-solar --candidate /path/to/candidate-solar \
  --source testdata/Optimism.sol --mode open --sessions 8 --samples 16 \
  --output /path/to/rpc-results.json
```

## Criterion measurements

Build each revision with `cargo bench -p solar-lsp --bench lsp --features bench --no-run`, preserve the emitted benchmark binaries, and run each binary with:

```sh
BINARY --bench --noplot --warm-up-time 1 --measurement-time 3 --sample-size 30 \
  --save-baseline NAME 'lsp/(folding-range|open-document-folding-range)/'
```

The first-request Optimism estimate is 44.932 ms before and 30.982 ms after (31.0% lower latency). Already cached requests remain around 26–27 µs. The complete logs are included beside the JSON; no benchmark source was changed.

Regenerate the chart with `uv run --no-project --with matplotlib python plot.py`. It reads only the recorded measurements.
