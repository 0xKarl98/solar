#!/usr/bin/env python3
"""Plot the saved main/candidate Criterion results and preserve their provenance."""

import hashlib
import json
import math
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.patches import Patch


ROOT = Path(__file__).resolve().parent
CHART_CASES = [
    ("lsp/member-completion/1024", "Member completion · 1,024 accesses"),
    ("lsp/open-document-selection-range-line-layout/minified-ascii", "Selection range · minified ASCII"),
    ("lsp/open-document-selection-range-line-layout/minified-unicode", "Selection range · minified Unicode"),
    ("lsp/open-document-selection-range-line-layout/multiline", "Selection range · multiline"),
    ("lsp/completion/all", "Completion · empty prefix"),
    ("lsp/completion/selective", "Completion · selective prefix"),
    ("lsp/completion/no-match", "Completion · no match"),
]
GRAY = "#8996A4"
TEAL = "#008C95"
RED = "#C64B43"
INK = "#23313D"
MUTED = "#62717D"


def read_json(path):
    return json.loads(path.read_text())


def source(path):
    return {
        "path": path.relative_to(ROOT).as_posix(),
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
    }


def load_run(name, metadata):
    runs = [run for run in metadata["runs"] if run["name"] == name]
    if len(runs) != 1 or runs[0]["exit_code"] != 0:
        raise RuntimeError(f"Need one completed successful {name} run")
    run = runs[0]
    expected_commit = metadata["baseline_commit" if name.startswith("baseline") else "candidate_commit"]
    if run["source_commit"] != expected_commit:
        raise RuntimeError(f"Unexpected source commit for {name}")
    binary = ROOT / ("baseline-lsp" if name.startswith("baseline") else "candidate-lsp")
    if binary.exists() and hashlib.sha256(binary.read_bytes()).hexdigest() != run["binary_sha256"]:
        raise RuntimeError(f"Saved binary does not match {name} metadata")
    cases = {}
    for benchmark_path in sorted((ROOT / name / "criterion").rglob("new/benchmark.json")):
        directory = benchmark_path.parent
        benchmark = read_json(benchmark_path)
        full_id = benchmark["full_id"]
        if full_id in cases:
            raise RuntimeError(f"Duplicate result: {name}/{full_id}")
        estimates_path = directory / "estimates.json"
        estimates = read_json(estimates_path)
        statistic = "slope" if estimates.get("slope") is not None else "mean"
        estimate = estimates[statistic]
        point = estimate["point_estimate"]
        if not math.isfinite(point) or point <= 0:
            raise RuntimeError(f"Invalid {statistic}: {name}/{full_id}")
        sample_path = directory / "sample.json"
        samples = read_json(sample_path)
        cases[full_id] = {
            "commit": run["source_commit"],
            "statistic": statistic,
            "estimate_ns": estimate,
            "time_us": point / 1000,
            "sample_count": len(samples["iters"]),
            "sources": {
                "benchmark": source(benchmark_path),
                "estimates": source(estimates_path),
                "samples": source(sample_path),
            },
        }
    return cases


def compare(baseline, candidate):
    if baseline.keys() != candidate.keys():
        raise RuntimeError(f"Unpaired cases: {baseline.keys() ^ candidate.keys()}")
    comparisons = []
    for case in sorted(baseline):
        base = baseline[case]
        new = candidate[case]
        if base["statistic"] != new["statistic"]:
            raise RuntimeError(f"Different statistics selected for {case}")
        ratio = new["estimate_ns"]["point_estimate"] / base["estimate_ns"]["point_estimate"]
        comparisons.append({
            "benchmark": case,
            "baseline": base,
            "candidate": new,
            "candidate_percent_of_main": ratio * 100,
            "change_percent": (ratio - 1) * 100,
        })
    return comparisons


def setting(run, option):
    command = run["command"]
    return command[command.index(option) + 1]


def main():
    metadata_path = ROOT / "run-metadata.json"
    metadata = read_json(metadata_path)
    comparisons = compare(load_run("baseline", metadata), load_run("candidate", metadata))
    if len(comparisons) != 13:
        raise RuntimeError(f"Expected 13 primary comparisons, found {len(comparisons)}")
    by_case = {item["benchmark"]: item for item in comparisons}
    missing = {case for case, _ in CHART_CASES} - by_case.keys()
    if missing:
        raise RuntimeError(f"Missing chart cases: {missing}")
    completed_names = {run["name"] for run in metadata["runs"] if run["exit_code"] == 0}
    repeats = []
    if {"baseline-repeat", "candidate-repeat"} <= completed_names:
        repeats = compare(load_run("baseline-repeat", metadata), load_run("candidate-repeat", metadata))
    evidence = {
        "schema_version": 1,
        "scope": "Local in-process member-completion and selection-range queries; editor transport and scheduling are excluded.",
        "statistics": "Criterion slope estimates when available, otherwise mean estimates; nanoseconds per iteration. Original confidence intervals are retained unchanged.",
        "change_formula": "100 * (candidate point estimate / baseline point estimate - 1)",
        "chart_note": "Each main estimate is normalized to 100. Colours indicate the direction of the point-estimate change, not statistical significance. All primary cases are included below; seven selected cases appear in the chart.",
        "run_metadata_source": source(metadata_path),
        "run_metadata": metadata,
        "comparisons": comparisons,
        "repeat_comparisons": repeats,
        "chart_cases": [case for case, _ in CHART_CASES],
    }
    plt.rcParams.update({
        "font.family": "DejaVu Sans",
        "font.size": 11,
        "text.color": INK,
        "axes.labelcolor": MUTED,
        "xtick.color": MUTED,
        "ytick.color": INK,
        "figure.facecolor": "white",
        "axes.facecolor": "white",
    })
    fig, ax = plt.subplots(figsize=(13.4, 8.7))
    fig.subplots_adjust(left=0.32, right=0.975, bottom=0.17, top=0.82)
    largest = max(100, *(by_case[case]["candidate_percent_of_main"] for case, _ in CHART_CASES))
    ax.set_xlim(0, largest * 1.60)
    ax.set_ylim(len(CHART_CASES) - 0.48, -0.65)
    ax.set_axisbelow(True)
    ax.grid(axis="x", color="#E9EDF0", linewidth=0.8)
    ax.set_xticks([0, 25, 50, 75, 100])
    ax.set_xlabel("Time per request or query · main = 100 · lower is better", labelpad=13)
    ax.set_yticks(range(len(CHART_CASES)), [label for _, label in CHART_CASES])
    ax.tick_params(axis="both", length=0)
    ax.tick_params(axis="y", pad=12)
    for spine in ax.spines.values():
        spine.set_visible(False)
    for row, (case, _) in enumerate(CHART_CASES):
        item = by_case[case]
        change = item["change_percent"]
        color = RED if change > 0 else TEAL
        ax.barh(row - 0.16, 100, height=0.25, color=GRAY)
        ax.barh(row + 0.16, item["candidate_percent_of_main"], height=0.25, color=color)
        ax.text(103, row - 0.16, f'{item["baseline"]["time_us"]:.3f} µs', va="center", color=MUTED)
        change_label = f"{change:+.1f}%"
        if abs(change) < 0.05:
            change_label = f"{change:+.2f}%"
        ax.text(item["candidate_percent_of_main"] + 3, row + 0.16,
                f'{item["candidate"]["time_us"]:.3f} µs  ({change_label})',
                va="center", color=color, weight="bold")
    baseline_sha = metadata["baseline_commit"][:9]
    candidate_sha = metadata["candidate_commit"][:9]
    fig.text(0.045, 0.952, "LSP benchmark results", fontsize=24, weight="bold")
    fig.text(0.045, 0.912, "Local in-process timings · normalized per case", fontsize=12, color=MUTED)
    fig.legend(handles=[
        Patch(color=GRAY, label=f"Main {baseline_sha}"),
        Patch(color=TEAL, label=f"Candidate {candidate_sha}"),
        Patch(color=RED, label="Candidate slower"),
    ], loc="upper left", bbox_to_anchor=(0.04, 0.89), ncol=3, frameon=False, fontsize=11)
    primary_runs = [run for run in metadata["runs"] if run["name"] in {"baseline", "candidate"}]
    settings = [{option: setting(run, option) for option in ["--sample-size", "--warm-up-time", "--measurement-time"]} for run in primary_runs]
    if settings[0] != settings[1]:
        raise RuntimeError("Primary runs used different measurement settings")
    method = settings[0]
    host = "macOS arm64" if metadata["platform"].startswith("macOS") and "arm64" in metadata["platform"] else metadata["platform"]
    fig.text(0.045, 0.071,
             f'{host} · Cargo bench profile · {method["--sample-size"]} samples · '
             f'{method["--warm-up-time"]} s warmup + {method["--measurement-time"]} s requested measurement',
             fontsize=10, color=MUTED)
    fig.text(0.045, 0.043,
             "Criterion point estimates; confidence intervals, all 13 cases and repeat controls are in the linked evidence.",
             fontsize=10, color=MUTED)
    fig.savefig(ROOT / "lsp-performance.png", dpi=180, facecolor="white")
    plt.close(fig)
    evidence["chart_source"] = source(ROOT / "lsp-performance.png")
    evidence["plot_script_source"] = source(Path(__file__).resolve())
    (ROOT / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    for item in comparisons:
        print(f'{item["benchmark"]}: {item["baseline"]["time_us"]:.3f} -> '
              f'{item["candidate"]["time_us"]:.3f} µs ({item["change_percent"]:+.2f}%)')
    print(f"Saved {len(comparisons)} primary comparisons and {len(repeats)} repeat comparisons")


if __name__ == "__main__":
    main()
