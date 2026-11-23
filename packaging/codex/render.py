"""Render an unpublished local Codex catalog from a verified macOS release archive."""

import argparse
import hashlib
from pathlib import Path
import re
import shutil
import tarfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("sha256", help="expected SHA-256 from the native release build")
    parser.add_argument("output", type=Path, help="new local catalog directory")
    args = parser.parse_args()
    pattern = r"pigeonmq-[0-9]+\.[0-9]+\.[0-9]+-(aarch64|x86_64)-apple-darwin\.tar\.gz"
    if not re.fullmatch(pattern, args.archive.name):
        parser.error("use a macOS PigeonMQ release archive")
    if not re.fullmatch(r"[0-9a-f]{64}", args.sha256):
        parser.error("supply a lowercase SHA-256 digest")
    with args.archive.open("rb") as source:
        digest = hashlib.sha256()
        while chunk := source.read(65536):
            digest.update(chunk)
        if digest.hexdigest() != args.sha256:
            parser.error("release archive digest does not match")
        source.seek(0)
        with tarfile.open(fileobj=source, mode="r:gz") as archive:
            members = archive.getmembers()
            expected = {"pigeonmq", "pigeonmq-dot-resolve", "LICENSE"}
            if (len(members) != len(expected) or {item.name for item in members} != expected
                    or any(not item.isfile() or item.size == 0 for item in members)):
                parser.error("unexpected release archive layout")
            args.output.mkdir(mode=0o700)
            try:
                shutil.copytree(Path(__file__).parent / "catalog", args.output, dirs_exist_ok=True)
                plugin = args.output / "plugins/pigeonmq-preview"
                (plugin / "bin").mkdir()
                for name, mode in [("bin/pigeonmq", 0o755), ("LICENSE", 0o644)]:
                    target = plugin / name
                    with archive.extractfile(target.name) as contents, target.open("xb") as destination:
                        shutil.copyfileobj(contents, destination)
                    target.chmod(mode)
            except BaseException:
                shutil.rmtree(args.output)
                raise


if __name__ == "__main__":
    main()
