# LSP member and position query benchmarks

Official main: `1cddb0736eddae7fc28c9c05372d2881d17d378f`. Candidate: `d24278df929ce0d660b3e14bd75b4893763f491e`.

The primary runs measured main, then the candidate, sequentially in the same
checkout on macOS arm64. The ordinary completion controls were repeated in the
reverse order. Each case uses 40 Criterion samples, a 1 second warmup and a
2 second requested measurement window, with Cargo's bench profile.

Both builds use the same benchmark source (SHA-256
`52d831e2e3544d219715812c4f12062992b63a3ef6993e20784f4bceb2bc56e7`). The new benchmark fixtures were overlaid on
main without production changes; `baseline-fixture.diff` records that patch.
The candidate source is unmodified. `run-metadata.json` records the revisions,
commands, executable hashes and Rust version. `measure.py` reproduces the runs.

The chart shows slope estimates normalized to main = 100%. The tables below
retain absolute times. Raw estimates, 95% confidence intervals, samples and
logs are included for every case. These timings cover warm in-process queries
and separate analysis builds; they exclude editor transport and scheduling.
Selection-range timing includes the query path and response construction.

## Primary comparison

| Case | Main (µs) | Candidate (µs) | Change |
|---|---:|---:|---:|
| lsp/completion/all | 60.864 | 60.226 | -1.0% |
| lsp/completion/no-match | 2.615 | 3.168 | +21.2% |
| lsp/completion/selective | 3.571 | 3.770 | +5.6% |
| lsp/member-completion/1024 | 1.305 | 0.157 | -88.0% |
| lsp/member-completion/256 | 0.426 | 0.146 | -65.7% |
| lsp/member-completion/64 | 0.194 | 0.139 | -28.5% |
| lsp/open-document-selection-range-line-layout/minified-ascii | 883.505 | 1.162 | -99.9% |
| lsp/open-document-selection-range-line-layout/minified-unicode | 1060.410 | 112.375 | -89.4% |
| lsp/open-document-selection-range-line-layout/multiline | 2.192 | 1.048 | -52.2% |
| lsp/open-document-selection-range/unifap-v2-router | 1.217 | 0.732 | -39.9% |
| lsp/open-document-selection-range/uniswap-v3 | 1.402 | 0.908 | -35.2% |
| lsp/project-analysis-after-edit/unifap-v2 | 20343.746 | 21142.944 | +3.9% |
| lsp/project-analysis/unifap-v2 | 20373.879 | 20274.582 | -0.5% |

## Ordinary completion, reverse-order repeat

| Case | Main (µs) | Candidate (µs) | Change |
|---|---:|---:|---:|
| lsp/completion/all | 63.804 | 63.135 | -1.0% |
| lsp/completion/no-match | 3.194 | 3.075 | -3.7% |
| lsp/completion/selective | 3.749 | 3.678 | -1.9% |

No-match completion is slower in the primary run and faster in the reverse-order
repeat. No improvement is claimed for ordinary completion. The chart includes
all three ordinary completion controls from the primary run.

![LSP query times](lsp-performance.png)
