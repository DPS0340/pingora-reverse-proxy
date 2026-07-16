#!/usr/bin/env python3
"""Prove that the installed JupyterHub distribution matches a pinned source tree."""

from __future__ import annotations

import importlib.util
import pathlib
import sys


def compare_tree(source: pathlib.Path, installed: pathlib.Path, pattern: str) -> int:
    checked = 0
    for source_file in sorted(source.rglob(pattern)):
        if not source_file.is_file() or "__pycache__" in source_file.parts:
            continue
        relative = source_file.relative_to(source)
        installed_file = installed / relative
        if not installed_file.is_file():
            raise SystemExit(f"installed JupyterHub is missing {installed_file}")
        if source_file.read_bytes() != installed_file.read_bytes():
            raise SystemExit(f"installed JupyterHub differs from pinned source at {relative}")
        checked += 1
    if checked == 0:
        raise SystemExit(f"pinned JupyterHub source has no files matching {pattern}")
    return checked


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} SOURCE_ROOT")
    source_root = pathlib.Path(sys.argv[1]).resolve(strict=True)
    source_package = source_root / "jupyterhub"
    source_templates = source_root / "share" / "jupyterhub" / "templates"

    spec = importlib.util.find_spec("jupyterhub")
    if spec is None or spec.origin is None:
        raise SystemExit("installed JupyterHub package is unavailable")
    installed_package = pathlib.Path(spec.origin).resolve(strict=True).parent
    installed_templates = pathlib.Path(sys.prefix) / "share" / "jupyterhub" / "templates"

    python_count = compare_tree(source_package, installed_package, "*.py")
    template_count = compare_tree(source_templates, installed_templates, "*")
    print(
        f"verified JupyterHub source identity: "
        f"python_files={python_count} templates={template_count}"
    )


if __name__ == "__main__":
    main()
