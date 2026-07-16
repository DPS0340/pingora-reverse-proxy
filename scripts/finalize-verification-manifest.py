#!/usr/bin/env python3
"""Durably append the verifier result while preserving the original exit status."""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 4:
        print(
            "usage: finalize-verification-manifest.py MANIFEST STARTED_NS STATUS",
            file=sys.stderr,
        )
        return 2
    manifest = Path(sys.argv[1])
    try:
        started_ns = int(sys.argv[2])
        original_status = int(sys.argv[3])
    except ValueError:
        print("started time and status must be integers", file=sys.stderr)
        return 2
    if started_ns < 0 or not 0 <= original_status <= 255:
        print("started time or status is outside its valid range", file=sys.stderr)
        return 2

    elapsed_ms = max(0, (time.monotonic_ns() - started_ns) // 1_000_000)
    try:
        with manifest.open("a", encoding="utf-8") as output:
            output.write(
                f"result\tstatus\t{original_status}"
                f"\telapsed_milliseconds\t{elapsed_ms}\n"
                "complete\tCOMPLETE\n"
            )
            output.flush()
            os.fsync(output.fileno())
    except OSError as error:
        print(f"failed to finalize verification manifest: {error}", file=sys.stderr)
        return original_status or 1
    return original_status


if __name__ == "__main__":
    raise SystemExit(main())
