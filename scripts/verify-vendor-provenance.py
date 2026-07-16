#!/usr/bin/env python3
"""Verify vendored Pingora source against a checked-in crates.io archive."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tarfile
from pathlib import Path, PurePosixPath


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def parse_inline_patch_path(cargo_toml: str, package: str) -> str:
    section = re.search(
        r"(?ms)^\[patch\.crates-io\]\s*$\n(?P<body>.*?)(?=^\[|\Z)", cargo_toml
    )
    if section is None:
        raise ValueError("Cargo.toml is missing [patch.crates-io]")
    entries = re.findall(
        rf"(?m)^\s*{re.escape(package)}\s*=\s*\{{\s*path\s*=\s*\"([^\"\n]+)\"\s*\}}\s*$",
        section.group("body"),
    )
    if len(entries) != 1:
        raise ValueError(
            f"[patch.crates-io] must contain exactly one literal inline {package} path entry"
        )
    return entries[0]


def parse_package_identity(manifest: str) -> tuple[str, str]:
    section = re.search(r"(?ms)^\[package\]\s*$\n(?P<body>.*?)(?=^\[|\Z)", manifest)
    if section is None:
        raise ValueError("vendored Cargo.toml is missing [package]")
    values: dict[str, str] = {}
    for key in ("name", "version"):
        match = re.search(
            rf"(?m)^\s*{key}\s*=\s*\"([^\"\n]+)\"\s*$", section.group("body")
        )
        if match is None:
            raise ValueError(f"vendored Cargo.toml is missing a literal package.{key}")
        values[key] = match.group(1)
    return values["name"], values["version"]


def regular_files(root: Path) -> dict[str, bytes]:
    files: dict[str, bytes] = {}
    for path in root.rglob("*"):
        if path.is_symlink():
            raise ValueError(f"vendored symlink is forbidden: {path}")
        if path.is_file():
            files[path.relative_to(root).as_posix()] = path.read_bytes()
    return files


def archive_files(archive: Path, crate: str, version: str) -> dict[str, bytes]:
    expected_root = f"{crate}-{version}"
    files: dict[str, bytes] = {}
    with tarfile.open(archive, "r:gz") as handle:
        for member in handle.getmembers():
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts or not path.parts:
                raise ValueError(f"unsafe archive member: {member.name}")
            if path.parts[0] != expected_root:
                raise ValueError(f"unexpected archive root: {member.name}")
            if member.isdir():
                continue
            if not member.isfile():
                raise ValueError(f"non-regular archive member: {member.name}")
            relative = PurePosixPath(*path.parts[1:]).as_posix()
            if not relative or relative in files:
                raise ValueError(f"invalid or duplicate archive member: {member.name}")
            stream = handle.extractfile(member)
            if stream is None:
                raise ValueError(f"cannot read archive member: {member.name}")
            files[relative] = stream.read()
    return files


def verify(metadata_path: Path, archive: Path, vendor_dir: Path, cargo_toml: Path) -> None:
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    archive_bytes = archive.read_bytes()
    actual_archive_digest = digest(archive_bytes)
    if actual_archive_digest != metadata["archive_sha256"]:
        raise ValueError(
            f"archive SHA-256 mismatch: {actual_archive_digest} != {metadata['archive_sha256']}"
        )

    upstream = archive_files(archive, metadata["crate"], metadata["version"])
    vendored = regular_files(vendor_dir)
    missing = set(metadata["allowed_missing"])
    patched = set(metadata["patched_files"])
    if missing & patched:
        raise ValueError("a path cannot be both missing and patched")
    if not missing <= set(upstream):
        raise ValueError(f"unknown allowed_missing entries: {sorted(missing - set(upstream))}")
    if not patched <= set(upstream):
        raise ValueError(f"unknown patched entries: {sorted(patched - set(upstream))}")

    expected_vendored = set(upstream) - missing
    if set(vendored) != expected_vendored:
        raise ValueError(
            f"vendored inventory mismatch: missing={sorted(expected_vendored - set(vendored))} "
            f"extra={sorted(set(vendored) - expected_vendored)}"
        )

    for name in sorted(expected_vendored - patched):
        if vendored[name] != upstream[name]:
            raise ValueError(f"unapproved vendored modification: {name}")

    for name, expected in metadata["patched_files"].items():
        upstream_digest = digest(upstream[name])
        vendored_digest = digest(vendored[name])
        if upstream_digest != expected["upstream_sha256"]:
            raise ValueError(f"upstream patched-file SHA-256 mismatch: {name}")
        if vendored_digest != expected["vendored_sha256"]:
            raise ValueError(f"vendored patched-file SHA-256 mismatch: {name}")
        if upstream_digest == vendored_digest:
            raise ValueError(f"declared patch is byte-identical to upstream: {name}")

    expected_path = f"vendor/{metadata['vendor_dir']}"
    actual_path = parse_inline_patch_path(
        cargo_toml.read_text(encoding="utf-8"), metadata["crate"]
    )
    if actual_path != expected_path:
        raise ValueError(f"Cargo patch must be exactly {{'path': '{expected_path}'}}")

    package_name, package_version = parse_package_identity(
        vendored["Cargo.toml"].decode("utf-8")
    )
    if package_name != metadata["crate"] or package_version != metadata["version"]:
        raise ValueError("vendored package identity does not match provenance metadata")


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata", type=Path, default=root / "vendor/vendor-provenance.json")
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--vendor-dir", type=Path)
    parser.add_argument("--cargo-toml", type=Path, default=root / "Cargo.toml")
    args = parser.parse_args()
    metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
    archive = args.archive or args.metadata.parent / metadata["archive"]
    vendor_dir = args.vendor_dir or args.metadata.parent / metadata["vendor_dir"]
    try:
        verify(args.metadata, archive, vendor_dir, args.cargo_toml)
    except (OSError, ValueError, KeyError, UnicodeDecodeError, json.JSONDecodeError) as error:
        print(f"vendor provenance verification failed: {error}", file=sys.stderr)
        return 1
    print(
        f"vendor provenance verified: {metadata['crate']} {metadata['version']} "
        f"archive_sha256={metadata['archive_sha256']}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
