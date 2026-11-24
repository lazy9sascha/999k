#!/usr/bin/env python3
"""Verify the Orca folder archive payload and reproducibility."""

import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "plugins" / "orca-999k"


class OrcaReleasePackage(unittest.TestCase):
    def test_archive_contains_only_versioned_runtime_files(self):
        version = json.loads((SOURCE / "orca-plugin.json").read_text())["version"]
        script = ROOT / "packaging" / "package_orca.py"
        with tempfile.TemporaryDirectory() as scratch:
            archives = []
            for directory in ("one", "two"):
                command = [sys.executable, script, version, Path(scratch) / directory]
                result = subprocess.run(command, capture_output=True, text=True, check=True)
                archives.append(Path(result.stdout.strip()))
            self.assertEqual(archives[0].read_bytes(), archives[1].read_bytes())
            folder = f"999k-orca-{version}"
            self.assertEqual(archives[0].name, f"{folder}.tar.gz")
            files = {
                f"{folder}/orca-plugin.json": SOURCE / "orca-plugin.json",
                f"{folder}/panel.tsx": SOURCE / "panel.tsx",
                f"{folder}/setup.ts": SOURCE / "setup.ts",
                f"{folder}/worker.ts": SOURCE / "worker.ts",
                f"{folder}/999k-field-radio-ink.png": SOURCE / "999k-field-radio-ink.png",
                f"{folder}/LICENSE": ROOT / "LICENSE",
            }
            with tarfile.open(archives[0], "r:gz") as tar:
                self.assertCountEqual(tar.getnames(), files)
                for name, source in files.items():
                    member = tar.getmember(name)
                    self.assertTrue(member.isfile())
                    self.assertEqual(member.mode, 0o644)
                    with tar.extractfile(member) as payload:
                        self.assertEqual(payload.read(), source.read_bytes())
            mismatch = subprocess.run(
                [sys.executable, script, "999.999.999", Path(scratch) / "bad"],
                capture_output=True, text=True,
            )
            self.assertEqual(mismatch.returncode, 1)
            self.assertFalse((Path(scratch) / "bad").exists())


if __name__ == "__main__":
    unittest.main()
