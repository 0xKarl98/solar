#!/usr/bin/env python3

from __future__ import annotations

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW_PATH = ROOT / ".github/workflows/lsp-bench.yml"
CHECKOUT_ACTION = "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
RUST_ACTION = "dtolnay/rust-toolchain@6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772"
UPLOAD_ACTION = "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a"
SERVER_ARGS = (
    "--server solar --server asyncswap "
    "--server nomic-foundation --server solc"
)


def workflow() -> str:
    if not WORKFLOW_PATH.is_file():
        raise AssertionError(f"workflow is missing: {WORKFLOW_PATH}")
    return WORKFLOW_PATH.read_text(encoding="utf-8")


def job_block(name: str) -> str:
    jobs = workflow().split("\njobs:\n", 1)[1]
    match = re.search(
        rf"^  {re.escape(name)}:\n(?P<body>.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)",
        jobs,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise AssertionError(f"job {name!r} is missing")
    return match.group(0)


class CrossServerWorkflowTests(unittest.TestCase):
    def test_triggers_are_unprivileged_and_bounded(self) -> None:
        text = workflow()
        header = text.split("\njobs:\n", 1)[0]
        pr = job_block("pr-smoke")
        full = job_block("full")

        self.assertIn("\n  pull_request:\n", header)
        self.assertIn("\n  workflow_dispatch:\n", header)
        self.assertNotIn("pull_request_target", header)
        self.assertNotIn("issue_comment", header)
        self.assertNotIn("schedule:", header)
        self.assertIn("\npermissions: {}\n", header)
        self.assertIn("if: github.event_name == 'pull_request'", pr)
        self.assertIn("continue-on-error: true", pr)
        self.assertIn("runs-on: ubuntu-24.04", pr)
        self.assertIn("contents: read", pr)
        self.assertIn("cancel-in-progress: true", pr)
        self.assertIn(
            "if: github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main'",
            full,
        )
        self.assertIn("runs-on: ubuntu-24.04", full)
        self.assertIn("timeout-minutes: 360", full)
        self.assertIn("contents: read", full)
        self.assertIn("cancel-in-progress: false", full)
        self.assertNotIn("continue-on-error: true", full)

    def test_both_jobs_use_pinned_read_only_checkouts_and_release_binaries(self) -> None:
        text = workflow()

        self.assertEqual(text.count(f"uses: {CHECKOUT_ACTION}"), 2)
        self.assertEqual(text.count(f"uses: {RUST_ACTION}"), 2)
        self.assertEqual(text.count("persist-credentials: false"), 2)
        self.assertEqual(text.count('toolchain: "1.96"'), 2)
        self.assertEqual(
            text.count("cargo build --locked --release -p solar-lsp-bench"), 2
        )
        self.assertEqual(
            text.count(
                "cargo build --locked --release -p solar-compiler --bin solar"
            ),
            2,
        )
        self.assertNotIn("pull_request_target", text)
        self.assertNotIn("github-script", text)

    def test_pr_smoke_is_reference_only_and_synthetic(self) -> None:
        pr = job_block("pr-smoke")

        self.assertIn(f"prepare --fixture synthetic {SERVER_ARGS}", pr)
        self.assertIn("run \\\n            --profile pr-smoke", pr)
        self.assertIn(SERVER_ARGS, pr)
        self.assertIn("--allow-failures", pr)
        self.assertIn("--solar-binary target/release/solar", pr)
        self.assertIn('--solar-revision "$(git rev-parse HEAD)"', pr)
        self.assertIn("validate-results \\\n            --profile pr-smoke", pr)
        self.assertIn("report \\\n            --input target/lsp-bench/pr-smoke/summary.json", pr)
        self.assertNotIn("doctor --publish", pr)
        self.assertNotIn("--require-authoritative", pr)

    def test_manual_full_is_strict_and_fixed_to_core4(self) -> None:
        full = job_block("full")
        run_step = full.split("      - name: Run full comparison\n", 1)[1].split(
            "\n      - ", 1
        )[0]

        self.assertIn(f"prepare {SERVER_ARGS}", full)
        self.assertIn(f"doctor {SERVER_ARGS}", full)
        self.assertIn("run \\\n            --profile full", run_step)
        self.assertIn(SERVER_ARGS, run_step)
        self.assertNotIn("--allow-failures", run_step)
        self.assertIn("--solar-binary target/release/solar", run_step)
        self.assertIn('--solar-revision "$(git rev-parse HEAD)"', run_step)
        self.assertIn("validate-results \\\n            --profile full", full)
        self.assertIn("report \\\n            --input target/lsp-bench/full/summary.json", full)
        self.assertNotIn("doctor --publish", full)
        self.assertNotIn("--require-authoritative", full)

    def test_only_validated_reports_and_raw_evidence_are_published(self) -> None:
        text = workflow()
        pr = job_block("pr-smoke")
        full = job_block("full")

        self.assertEqual(text.count(f"uses: {UPLOAD_ACTION}"), 2)
        for job, output, retention in (
            (pr, "target/lsp-bench/pr-smoke", 30),
            (full, "target/lsp-bench/full", 90),
        ):
            with self.subTest(output=output):
                self.assertIn(
                    f"if: always() && hashFiles('{output}/summary.json') != ''", job
                )
                self.assertIn(
                    "id: validate\n",
                    job,
                )
                self.assertIn(
                    "if: always() && steps.validate.outcome == 'success' "
                    f"&& hashFiles('{output}/summary.json') != ''",
                    job,
                )
                self.assertIn(
                    "if: always() && steps.validate.outcome == 'success' "
                    f"&& hashFiles('{output}/COMPARISON.md') != ''",
                    job,
                )
                self.assertIn('>> "$GITHUB_STEP_SUMMARY"', job)
                self.assertIn("if: always()", job)
                for name in ("summary.json", "samples.json", "samples.jsonl", "COMPARISON.md"):
                    self.assertIn(f"{output}/{name}", job)
                self.assertNotIn(f"            {output}/\n", job)
                self.assertNotIn(f"{output}/summary.md", job)
                self.assertIn(f"retention-days: {retention}", job)
                self.assertIn("if-no-files-found: warn", job)
                self.assertIn("tools/lsp-bench/benchmark.yaml", job)
                self.assertIn("tools/lsp-bench/servers.lock.yaml", job)
                self.assertIn("tools/lsp-bench/fixtures.lock.yaml", job)
                self.assertIn("tools/lsp-bench/install/", job)
        self.assertIn("${{ github.run_id }}-${{ github.run_attempt }}", text)


if __name__ == "__main__":
    unittest.main()
