from pathlib import Path
import json

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import MultipleLocator


root = Path(__file__).resolve().parent
data = json.loads((root / "measurements.json").read_text())
protocol = data["workloads"][0]["metrics"]["p50"]
criterion = data["criterion"]["lsp/open-document-folding-range/optimism-first-request"]
panels = [
    ("Folding refresh over stdio", protocol["base_ms"], protocol["candidate_ms"]),
    ("First request · Criterion", criterion["base"]["estimate_ms"], criterion["candidate"]["estimate_ms"]),
]

plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 11})
fig, axes = plt.subplots(1, 2, figsize=(10.4, 3.8), facecolor="white")
fig.subplots_adjust(left=0.09, right=0.98, bottom=0.19, top=0.64, wspace=0.33)
fig.text(0.04, 0.94, "Optimism folding latency", fontsize=19, weight="bold", color="#172b3a")
fig.text(0.04, 0.86, "5.1 MiB source · 16,059 ranges · lower is better", color="#536575", fontsize=11)

for ax, (label, before, after) in zip(axes, panels):
    reduction = (1 - after / before) * 100
    ax.set_title(f"{label}\n{reduction:.1f}% less latency", loc="left", pad=15, fontsize=12, weight="bold", color="#172b3a")
    ax.barh([1, 0], [before, after], height=0.47, color=["#a8b4bf", "#168575"], zorder=3)
    for y, value in [(1, before), (0, after)]:
        ax.text(value + 1.1, y, f"{value:.2f}", va="center", fontsize=11, color="#172b3a")
    ax.set_yticks([1, 0], ["Before", "After"])
    ax.set_xlim(0, 62)
    ax.set_ylim(-0.55, 1.55)
    ax.xaxis.set_major_locator(MultipleLocator(20))
    ax.set_xlabel("Milliseconds", color="#536575", labelpad=9)
    ax.grid(axis="x", color="#e6ebef", linewidth=0.8, zorder=0)
    ax.tick_params(axis="both", length=0, colors="#536575", labelsize=10)
    for spine in ax.spines.values():
        spine.set_visible(False)

fig.savefig(root / "folding-latency.png", dpi=180, facecolor="white")
print(root / "folding-latency.png")
