#!/usr/bin/env python3
"""Verify and count the exact property-test runner inventory."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INVENTORY = ROOT / "scripts" / "property-runners.txt"


def load_inventory() -> set[tuple[str, str, str]]:
    entries: set[tuple[str, str, str]] = set()
    for line_number, raw in enumerate(INVENTORY.read_text(encoding="utf-8").splitlines(), 1):
        if not raw or raw.startswith("#"):
            continue
        parts = raw.split("|")
        if len(parts) != 3 or parts[0] not in {"macro", "manual"}:
            raise SystemExit(f"invalid property inventory line {line_number}: {raw!r}")
        mode, relative, name = parts
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
            raise SystemExit(f"invalid property runner name on line {line_number}: {name!r}")
        entry = (mode, relative, name)
        if entry in entries:
            raise SystemExit(f"duplicate property inventory entry: {raw}")
        entries.add(entry)
    if not entries:
        raise SystemExit("property runner inventory is empty")
    return entries


def discover_source_runners() -> set[tuple[str, str, str]]:
    discovered: set[tuple[str, str, str]] = set()
    macro_pattern = re.compile(r"(?ms)^proptest!\s*\{\s*\n(.*?)^\}\s*$")
    function_pattern = re.compile(
        r"(?ms)^fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(.*?^\}\s*$"
    )
    name_pattern = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")

    for path in sorted((ROOT / "tests").glob("*.rs")):
        relative = path.relative_to(ROOT).as_posix()
        source = path.read_text(encoding="utf-8")
        for block in macro_pattern.findall(source):
            for name in name_pattern.findall(block):
                discovered.add(("macro", relative, name))
        for match in function_pattern.finditer(source):
            if "TestRunner::default()" in match.group(0):
                discovered.add(("manual", relative, match.group(1)))
    return discovered


def verify_source() -> set[tuple[str, str, str]]:
    expected = load_inventory()
    actual = discover_source_runners()
    if actual != expected:
        missing = sorted(expected - actual)
        unexpected = sorted(actual - expected)
        raise SystemExit(
            f"property runner inventory mismatch: missing={missing!r} unexpected={unexpected!r}"
        )
    return expected


def verify_compiled_list(entries: set[tuple[str, str, str]]) -> None:
    compiled: dict[str, int] = {}
    for raw in sys.stdin:
        line = raw.strip()
        if not line.endswith(": test"):
            continue
        name = line[: -len(": test")]
        compiled[name] = compiled.get(name, 0) + 1

    errors = []
    for _, _, name in sorted(entries):
        count = compiled.get(name, 0)
        if count != 1:
            errors.append(f"{name} compiled-list count={count}, expected=1")
    if errors:
        raise SystemExit("; ".join(errors))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--count", action="store_true")
    parser.add_argument("--verify-list", action="store_true")
    args = parser.parse_args()
    if args.count == args.verify_list:
        parser.error("select exactly one of --count or --verify-list")

    entries = verify_source()
    if args.verify_list:
        verify_compiled_list(entries)
        print(f"property runner inventory verified: {len(entries)} compiled runners")
    else:
        print(len(entries))


if __name__ == "__main__":
    main()
