#!/usr/bin/env python3
from __future__ import annotations

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
VERIFIER = ROOT / "scripts/verify-vendor-provenance.py"
METADATA = ROOT / "vendor/vendor-provenance.json"
ARCHIVE = ROOT / "vendor/upstream/pingora-load-balancing-0.8.1.crate"
VENDOR = ROOT / "vendor/pingora-load-balancing-0.8.1"


class VendorProvenanceTests(unittest.TestCase):
    def run_verifier(self, archive: Path, vendor: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                "python3",
                str(VERIFIER),
                "--metadata",
                str(METADATA),
                "--archive",
                str(archive),
                "--vendor-dir",
                str(vendor),
                "--cargo-toml",
                str(ROOT / "Cargo.toml"),
            ],
            text=True,
            capture_output=True,
            check=False,
        )

    def fixture(self, directory: Path) -> tuple[Path, Path]:
        archive = directory / ARCHIVE.name
        vendor = directory / VENDOR.name
        shutil.copy2(ARCHIVE, archive)
        shutil.copytree(VENDOR, vendor)
        return archive, vendor

    def test_checked_in_vendor_matches_pinned_archive_and_patch_hashes(self) -> None:
        result = self.run_verifier(ARCHIVE, VENDOR)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_unapproved_change_to_upstream_identical_file(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            archive, vendor = self.fixture(Path(raw))
            (vendor / "LICENSE").write_text("tampered\n", encoding="utf-8")
            result = self.run_verifier(archive, vendor)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("unapproved vendored modification", result.stderr)

    def test_rejects_patch_drift(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            archive, vendor = self.fixture(Path(raw))
            with (vendor / "src/lib.rs").open("a", encoding="utf-8") as handle:
                handle.write("\n// drift\n")
            result = self.run_verifier(archive, vendor)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("vendored patched-file SHA-256 mismatch", result.stderr)

    def test_rejects_archive_drift(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            archive, vendor = self.fixture(Path(raw))
            with archive.open("ab") as handle:
                handle.write(b"drift")
            result = self.run_verifier(archive, vendor)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("archive SHA-256 mismatch", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
