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
EXPECTED_DIFFERENTIAL_CASES = 35
VENDOR_METADATA = json.loads(
    (ROOT / "vendor" / "vendor-provenance.json").read_text(encoding="utf-8")
)
EXPECTED_VENDOR_PROVENANCE = (
    VENDOR_METADATA["crate"],
    VENDOR_METADATA["version"],
    VENDOR_METADATA["archive_sha256"],
)
EXPECTED_JUPYTERHUB = "5.5.0"
EXPECTED_COMMIT = CONTRACT["EXPECTED_JUPYTERHUB_COMMIT"]
EXPECTED_SCENARIOS = set(CONTRACT["REQUIRED_SCENARIOS"])
EXPECTED_LOGS = {
    "01-fmt.log",
    "02-clippy.log",
    "03-tests.log",
    "04-differential.log",
    "05-jupyterhub.log",
    "06-container.log",
    "07-helm.log",
    "08-audit.log",
    "09-deny.log",
}


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
    vendor_provenance: tuple[str, str, str] | None = None
    summaries: list[dict[str, object]] = []
    logs = sorted(log_dir.glob("[0-9][0-9]-*.log"))
    observed_logs = {log.name for log in logs}
    if observed_logs != EXPECTED_LOGS:
        raise SystemExit(
            "verification log inventory mismatch: "
            f"missing={sorted(EXPECTED_LOGS - observed_logs)!r}, "
            f"extra={sorted(observed_logs - EXPECTED_LOGS)!r}"
        )
    for log in logs:
        text = log.read_text(encoding="utf-8", errors="replace")
        passed = sum(
            int(match.group(1))
            for match in re.finditer(r"test result: ok\. (\d+) passed", text)
        )
        rust_tests += passed
        if log.name == "04-differential.log":
            differential_cases += passed
        if log.name == "03-tests.log":
            matches = re.findall(
                r"^vendor provenance verified: ([a-z0-9-]+) ([0-9]+(?:\.[0-9]+){2}) "
                r"archive_sha256=([0-9a-f]{64})$",
                text,
                flags=re.MULTILINE,
            )
            if len(matches) != 1:
                raise SystemExit(
                    f"expected one vendor provenance marker, found {len(matches)}"
                )
            vendor_provenance = matches[0]
            if vendor_provenance != EXPECTED_VENDOR_PROVENANCE:
                raise SystemExit(
                    "vendor provenance marker does not match canonical metadata"
                )
        for line in text.splitlines() if log.name == "05-jupyterhub.log" else ():
            marker = "JUPYTERHUB_E2E_SUMMARY="
            if marker in line:
                value = json.loads(line.split(marker, 1)[1].strip())
                if not isinstance(value, dict):
                    raise SystemExit("JupyterHub summary is not an object")
                summaries.append(value)

    if rust_tests == 0:
        raise SystemExit("verification manifest found no passing Rust tests")
    if differential_cases != EXPECTED_DIFFERENTIAL_CASES:
        raise SystemExit(
            f"expected exactly {EXPECTED_DIFFERENTIAL_CASES} differential cases, "
            f"found {differential_cases}"
        )
    if vendor_provenance is None:
        raise SystemExit("verification manifest found no vendor provenance evidence")
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
        package, version, archive_sha256 = vendor_provenance
        output.write(
            f"provenance\tvendor\t{package}\t{version}\tarchive_sha256\t{archive_sha256}\n"
        )
        output.write(f"count\trust_test_passed\t{rust_tests}\n")
        output.write(f"count\tdifferential_cases\t{differential_cases}\n")
        output.write(f"count\tjupyterhub_runs\t{len(summaries)}\n")
        output.write(f"count\tjupyterhub_scenarios\t{scenario_total}\n")


if __name__ == "__main__":
    main()
