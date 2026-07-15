#!/usr/bin/env python3
"""Append fail-closed aggregate counts from canonical verification logs."""

from __future__ import annotations

import json
import re
import runpy
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONTRACT = runpy.run_path(str(ROOT / "scripts" / "jupyterhub-e2e.py"))
EXPECTED_BACKENDS = {"memory", "redis"}
EXPECTED_JUPYTERHUB = "5.5.0"
EXPECTED_COMMIT = CONTRACT["EXPECTED_JUPYTERHUB_COMMIT"]
EXPECTED_SCENARIOS = set(CONTRACT["REQUIRED_SCENARIOS"])


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: summarize-verification.py LOG_DIR MANIFEST")
    log_dir = Path(sys.argv[1])
    manifest = Path(sys.argv[2])
    if not log_dir.is_dir():
        raise SystemExit(f"verification log directory is missing: {log_dir}")
    if not manifest.is_file():
        raise SystemExit(f"verification manifest is missing: {manifest}")

    rust_tests = 0
    differential_cases = 0
    summaries: list[dict[str, object]] = []
    for log in sorted(log_dir.glob("[0-9][0-9]-*.log")):
        text = log.read_text(encoding="utf-8", errors="replace")
        passed = sum(
            int(match.group(1))
            for match in re.finditer(r"test result: ok\. (\d+) passed", text)
        )
        rust_tests += passed
        if log.name == "04-differential.log":
            differential_cases += passed
        for line in text.splitlines():
            marker = "JUPYTERHUB_E2E_SUMMARY="
            if marker in line:
                value = json.loads(line.split(marker, 1)[1].strip())
                if not isinstance(value, dict):
                    raise SystemExit("JupyterHub summary is not an object")
                summaries.append(value)

    if rust_tests == 0:
        raise SystemExit("verification manifest found no passing Rust tests")
    if differential_cases == 0:
        raise SystemExit("verification manifest found no differential cases")
    if len(summaries) != len(EXPECTED_BACKENDS):
        raise SystemExit(
            f"expected two JupyterHub summaries, found {len(summaries)}"
        )

    observed_backends: set[str] = set()
    scenario_total = 0
    for summary in summaries:
        backend = summary.get("backend")
        scenarios = summary.get("scenarios")
        if not isinstance(backend, str) or backend in observed_backends:
            raise SystemExit(f"invalid or duplicate JupyterHub backend: {backend!r}")
        observed_backends.add(backend)
        if summary.get("jupyterhub") != EXPECTED_JUPYTERHUB:
            raise SystemExit("JupyterHub summary version mismatch")
        if summary.get("jupyterhub_commit") != EXPECTED_COMMIT:
            raise SystemExit("JupyterHub summary source commit mismatch")
        if not isinstance(scenarios, list) or not all(
            isinstance(item, str) for item in scenarios
        ):
            raise SystemExit(f"invalid scenario list for backend {backend}")
        scenario_set = set(scenarios)
        if len(scenarios) != len(scenario_set) or scenario_set != EXPECTED_SCENARIOS:
            raise SystemExit(f"scenario inventory mismatch for backend {backend}")
        scenario_total += len(scenarios)

    if observed_backends != EXPECTED_BACKENDS:
        raise SystemExit(
            f"JupyterHub backend matrix mismatch: {sorted(observed_backends)!r}"
        )

    with manifest.open("a", encoding="utf-8") as output:
        output.write(f"count\trust_test_passed\t{rust_tests}\n")
        output.write(f"count\tdifferential_cases\t{differential_cases}\n")
        output.write(f"count\tjupyterhub_runs\t{len(summaries)}\n")
        output.write(f"count\tjupyterhub_scenarios\t{scenario_total}\n")


if __name__ == "__main__":
    main()
