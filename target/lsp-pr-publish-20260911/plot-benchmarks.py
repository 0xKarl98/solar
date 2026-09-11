#!/usr/bin/env python3
"""Plot the retained Criterion estimates without recomputing their statistics."""

import argparse
import hashlib
import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


ROOT = Path(__file__).resolve().parents[2]
OUT = Path(__file__).resolve().parent
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--baseline-name", default="before-20260911")
parser.add_argument("--candidate-name", default="after-20260911")
parser.add_argument("--baseline-commit", default="eaed45e6d6cf42de7c8059f391252ddc3bfd3774")
parser.add_argument("--candidate-commit", default="b8af5d6dea02ea7dae9111c92a58fad90290bdb4")
parser.add_argument("--warmup-seconds", type=float, default=1)
parser.add_argument("--measurement-seconds", type=float, default=1)
parser.add_argument("--provenance-note", default="Commit mapping, host and timing settings come from the completed task handoff; Criterion's named estimates do not embed Git revisions.")
args = parser.parse_args()
CASES = [
    ("lsp_unchanged-document-update/optimism", "Unchanged full-document update", "Optimism.sol · 5.38 MB", "ms", 1e6),
    ("lsp_completion/selective", "Completion · selective prefix", "256-function fixture · function_0255", "µs", 1e3),
    ("lsp_completion/no-match", "Completion · no matching symbols", "256-function fixture · not_a_symbol", "µs", 1e3),
    ("lsp_completion/all", "Completion · empty prefix (control)", "256-function fixture · return all symbols", "µs", 1e3),
]
ROLES = [("baseline", args.baseline_name, args.baseline_commit), ("candidate", args.candidate_name, args.candidate_commit)]
COLORS = {"baseline": "#73859A", "candidate": "#087F8C"}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


evidence = {
    "schema_version": 1,
    "statistic": "Criterion slope point estimate, nanoseconds per iteration",
    "confidence_level": 0.95,
    "comparison": "candidate / baseline - 1; slope point estimates only",
    "baseline_commit": args.baseline_commit,
    "candidate_commit": args.candidate_commit,
    "provenance_note": args.provenance_note,
    "reported_method": {"host": "macOS arm64", "warmup_seconds": args.warmup_seconds, "measurement_seconds": args.measurement_seconds},
    "scope": "In-process production notification handler and symbol-query benchmarks, not stdio or editor latency.",
    "cases": [],
}

plt.rcParams.update({
    "font.family": "DejaVu Sans", "font.size": 11,
    "axes.titlesize": 14, "axes.titleweight": "bold",
    "svg.fonttype": "none", "figure.facecolor": "#FFFFFF",
    "axes.facecolor": "#FFFFFF", "text.color": "#172B3A",
    "axes.labelcolor": "#536675", "xtick.color": "#536675",
    "ytick.color": "#172B3A", "axes.edgecolor": "#D9E1E7",
})
fig, axes = plt.subplots(2, 2, figsize=(13.8, 8.2))
fig.subplots_adjust(left=0.095, right=0.965, bottom=0.18, top=0.80, hspace=0.78, wspace=0.35)
fig.text(0.04, 0.945, "LSP hot-path benchmarks", fontsize=24, weight="bold")
fig.text(0.04, 0.90, "Criterion slope estimates with 95% confidence intervals · lower is better", fontsize=12, color="#536675")

for ax, (case, title, detail, unit, scale) in zip(axes.flat, CASES):
    item = {"benchmark": case.replace("lsp_", "lsp/", 1), "title": title, "unit": unit, "series": {}}
    for role, name, commit in ROLES:
        directory = ROOT / "target/criterion" / case / name
        estimates_path = directory / "estimates.json"
        sample_path = directory / "sample.json"
        estimates = json.loads(estimates_path.read_text())
        samples = json.loads(sample_path.read_text())
        slope = estimates["slope"]
        item["series"][role] = {
            "commit": commit,
            "source": str(estimates_path.relative_to(ROOT)),
            "source_sha256": sha256(estimates_path),
            "sample_source": str(sample_path.relative_to(ROOT)),
            "sample_sha256": sha256(sample_path),
            "sample_count": len(samples["iters"]),
            "slope_ns": slope,
        }
    base = item["series"]["baseline"]["slope_ns"]["point_estimate"]
    candidate = item["series"]["candidate"]["slope_ns"]["point_estimate"]
    change = (candidate / base - 1) * 100
    item["change_percent"] = change
    evidence["cases"].append(item)
    max_upper = max(x["slope_ns"]["confidence_interval"]["upper_bound"] / scale for x in item["series"].values())
    ax.set_xlim(0, max_upper * 1.30)
    for y, (role, _, _) in zip([1, 0], ROLES):
        slope = item["series"][role]["slope_ns"]
        point = slope["point_estimate"] / scale
        low = slope["confidence_interval"]["lower_bound"] / scale
        high = slope["confidence_interval"]["upper_bound"] / scale
        ax.barh(y, point, height=0.44, color=COLORS[role], zorder=3)
        ax.errorbar(point, y, xerr=[[point-low], [high-point]], fmt="none", ecolor="#172B3A", capsize=5, elinewidth=1.4, capthick=1.4, zorder=4)
        ax.text(high + max_upper*0.035, y, f"{point:.3f} {unit}", va="center", fontsize=12, weight="bold")
    ax.set_yticks([1, 0], ["Baseline", "Candidate"])
    ax.set_ylim(-0.55, 1.55)
    ax.tick_params(axis="both", length=0)
    ax.grid(axis="x", color="#E7EDF1", linewidth=0.8, zorder=0)
    for side in ["top", "right", "left"]:
        ax.spines[side].set_visible(False)
    ax.spines["bottom"].set_color("#D9E1E7")
    ax.set_xlabel(f"Time per iteration ({unit})", fontsize=10, labelpad=8)
    ax.text(0, 1.28, title, transform=ax.transAxes, fontsize=13, weight="bold")
    ax.text(0, 1.13, detail, transform=ax.transAxes, fontsize=10, color="#536675")
    ax.text(1, 1.13, f"{change:.1f}% time", transform=ax.transAxes, ha="right", fontsize=11, weight="bold", color=COLORS["candidate"])

sample_counts = sorted({series["sample_count"] for item in evidence["cases"] for series in item["series"].values()})
samples_text = "/".join(str(n) for n in sample_counts)
fig.text(0.04, 0.083, f"macOS arm64 · {samples_text} samples per case · {args.warmup_seconds:g} s warmup + {args.measurement_seconds:g} s measurement", fontsize=10.5, color="#536675")
fig.text(0.04, 0.048, f"Baseline {args.baseline_commit[:9]} → candidate {args.candidate_commit[:9]}. Local handler/query measurements; not end-to-end editor latency.", fontsize=10.5, color="#536675")
fig.savefig(OUT / "lsp-performance.png", dpi=200, facecolor="white")
fig.savefig(OUT / "lsp-performance.svg", facecolor="white")
(OUT / "benchmark-evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
for item in evidence["cases"]:
    print(item["benchmark"], f'{item["change_percent"]:.3f}%')
