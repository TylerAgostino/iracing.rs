import json
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch

import live_benchmarks as bench


def example(run_id="base", p50=10):
    return {
        "format_version": 1, "run_id": run_id, "git_sha": "a" * 40,
        "git_dirty": False, "profile": "test-desktop",
        "environment": {
            "os_version": "Windows 11 12345", "cpu_model": "CPU",
            "logical_cores": 8, "rustc": "rustc test", "target": "x86_64-pc-windows-msvc",
            "build_profile": "bench", "power_profile": "Balanced",
        },
        "scenario": {
            "source": "active-iracing", "declared_tick_hz": 60,
            "frame_size": 4096, "schema_fingerprint": "fnv1a64-0123456789abcdef",
            "warmup_frames": 120, "target_frames": 600, "workload_version": 1,
        },
        "cases": [{
            "id": "live_acquisition/owned_frame", "experiment_version": 2,
            "status": "complete", "samples": 600,
            "parameters": {"timed_boundary": "get_new_data/live_frame_snapshot"},
            "metrics": {"p50_us": p50, "p95_us": p50 * 2, "p99_us": p50 * 3},
        }],
    }


class LiveBenchmarkTest(unittest.TestCase):
    def test_parsing_and_signed_delta(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "run.json"
            path.write_text(json.dumps(example()), encoding="utf-8")
            base = bench.load(path)
            head = example("head", 8)
            result = bench.compare_runs(base, head)
            case = result["cases"][0]
            self.assertEqual(case["status"], "comparable")
            self.assertEqual(case["metrics"]["p50_us"]["change_pct"], -20)
            self.assertIn("-20.0%", bench.markdown(result))

    def test_mismatch_incomplete_and_unpaired(self):
        base, head = example(), example("head")
        head["scenario"]["frame_size"] += 1
        self.assertEqual(bench.compare_runs(base, head)["cases"][0]["status"], "incomparable")
        head = example("head")
        head["cases"][0]["experiment_version"] = 3
        self.assertEqual(bench.compare_runs(base, head)["cases"][0]["status"], "unpaired")
        head = example("head")
        head["cases"][0]["status"] = "incomplete"
        self.assertEqual(bench.compare_runs(base, head)["cases"][0]["status"], "incomparable")
        head = example("head")
        head["cases"][0]["id"] = "live_consumer/dynamic_1"
        head["cases"][0]["experiment_version"] = 1
        head["cases"][0]["parameters"] = {"subscribers": 1, "adapter": "DynamicFrame", "delivery": "latest-wins"}
        head["cases"][0]["metrics"]["subscribers"] = []
        self.assertEqual([c["status"] for c in bench.compare_runs(base, head)["cases"]], ["unpaired", "unpaired"])

    def test_baseline_resolution_and_sha_check(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(bench, "STORE", Path(directory)):
            store = Path(directory)
            (store / "runs").mkdir()
            (store / "runs/base.json").write_text(json.dumps(example()), encoding="utf-8")
            (store / "baselines.json").write_text(json.dumps({"current-main": {"run_id": "base", "git_sha": "a" * 40}}), encoding="utf-8")
            self.assertEqual(bench.resolve_baseline("current-main"), store / "runs/base.json")
            (store / "baselines.json").write_text(json.dumps({"current-main": {"run_id": "base", "git_sha": "b" * 40}}), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "SHA"):
                bench.resolve_baseline("current-main")

    def test_validation(self):
        run = example()
        bench.validate(run)
        run["cases"][0]["samples"] = 1
        with self.assertRaisesRegex(ValueError, "600"):
            bench.validate(run)

    def test_promotion_scrubs_raw_samples_and_refuses_overwrite(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(bench, "RESULTS", Path(directory)):
            run = example()
            run["cases"][0]["metrics"]["samples_ns"] = [123, 456]
            source = Path(directory) / "source.json"
            source.write_text(json.dumps(run), encoding="utf-8")
            bench.promote(Namespace(input=source))
            promoted = Path(directory) / run["profile"] / "base.json"
            self.assertNotIn("samples_ns", json.loads(promoted.read_text())["cases"][0]["metrics"])
            with self.assertRaises(FileExistsError):
                bench.promote(Namespace(input=source))


if __name__ == "__main__":
    unittest.main()
