#!/usr/bin/env python3
"""Package the reviewed Orca runtime files for folder installation."""

import argparse
import gzip
from io import BytesIO
import json
import os
from pathlib import Path
import re
import tarfile
from tempfile import TemporaryDirectory

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = "orca-plugin.json"
ENTRY_POINTS = {"worker.ts", "panel.tsx"}
SUPPORT_FILES = {"setup.ts", "999k-field-radio-ink.png"}


def package(version, output):
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("Expected a numbered release version.")
    source = ROOT / "plugins" / "p-999k"
    members = {}
    for filename in sorted({MANIFEST, *ENTRY_POINTS, *SUPPORT_FILES}):
        path = source / filename
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"Expected a regular Orca plugin file: {filename}")
        members[filename] = path.read_bytes()
        if not members[filename]:
            raise ValueError(f"Empty Orca plugin file: {filename}")
    manifest = json.loads(members[MANIFEST])
    if not isinstance(manifest, dict) or manifest.get("id") != "p-999k" or manifest.get("version") != version:
        raise ValueError("Orca plugin id or version does not match the release.")
    panels = manifest["contributes"]["panels"]
    if [manifest["main"], *[panel["entry"] for panel in panels]] != ["worker.ts", "panel.tsx"]:
        raise ValueError("Orca manifest entry points differ from the package allowlist.")
    members["LICENSE"] = (ROOT / "LICENSE").read_bytes()

    output.mkdir(parents=True, exist_ok=True)
    folder = f"p-999k-{version}"
    archive = output / f"{folder}.tar.gz"
    with TemporaryDirectory(prefix=".package-", dir=output) as scratch:
        staged = Path(scratch) / archive.name
        with staged.open("xb") as destination:
            with gzip.GzipFile(filename="", fileobj=destination, mode="wb", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                    for filename, content in sorted(members.items()):
                        info = tarfile.TarInfo(f"{folder}/{filename}")
                        info.mode = 0o644
                        info.size = len(content)
                        tar.addfile(info, BytesIO(content))
        os.link(staged, archive)
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        archive = package(args.version, args.output)
    except (KeyError, TypeError, ValueError, OSError) as error:
        parser.exit(1, f"Orca plugin packaging failed: {error}\n")
    print(archive)


if __name__ == "__main__":
    main()
