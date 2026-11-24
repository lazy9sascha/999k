#!/usr/bin/env python3
"""Create pinned installers from assembled, checked release archives."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import tarfile

TEMPLATES = Path(__file__).resolve().parent
RELEASE = "https://github.com/lazy9sascha/999k/releases/download"
BINARIES = ("999k", "999k-dot-resolve")
TARGETS = {
    "aarch64-apple-darwin": ("Darwin:arm64", "on_macos", "on_arm"),
    "x86_64-apple-darwin": ("Darwin:x86_64", "on_macos", "on_intel"),
    "x86_64-unknown-linux-gnu": ("Linux:x86_64", "on_linux", "on_intel"),
    "aarch64-unknown-linux-gnu": ("Linux:aarch64", "on_linux", "on_arm"),
}
OPTIONAL_TARGET = "aarch64-unknown-linux-gnu"


def sha256(stream):
    digest = hashlib.sha256()
    for block in iter(lambda: stream.read(1024 * 1024), b""):
        digest.update(block)
    return digest.hexdigest()


def read_archives(version, directory):
    pins = {}
    for line in (directory / "SHA256SUMS").read_text(encoding="ascii").splitlines():
        entry = re.fullmatch(r"([0-9a-f]{64})  (999k-" + re.escape(version) + r"-(.+)\.tar\.gz)", line)
        if not entry:
            raise ValueError("SHA256SUMS contains an invalid entry.")
        digest, name, target = entry.groups()
        if target not in TARGETS or target in pins:
            raise ValueError(f"Unexpected or duplicate release target: {target}")
        archive = directory / name
        with archive.open("rb") as stream:
            if sha256(stream) != digest:
                raise ValueError(f"SHA-256 mismatch for {name}")
            stream.seek(0)
            with tarfile.open(fileobj=stream, mode="r:gz") as tar:
                members = tar.getmembers()
                if len(members) != 3 or {member.name for member in members} != {*BINARIES, "LICENSE"}:
                    raise ValueError(f"Unexpected archive layout: {name}")
                for member in members:
                    if not member.isfile() or not member.size or (
                        member.name in BINARIES and not member.mode & 0o100
                    ):
                        raise ValueError(f"Invalid archive member {member.name}: {name}")
                file_pins = []
                for binary in BINARIES:
                    with tar.extractfile(binary) as stream:
                        file_pins.append(sha256(stream))
        pins[target] = (name, digest, *file_pins)
    missing = TARGETS.keys() - pins.keys() - {OPTIONAL_TARGET}
    if missing:
        raise ValueError(f"Missing release targets: {', '.join(sorted(missing))}")
    return pins


def shell_pins(pins):
    cases = []
    for target, (platform, _, _) in TARGETS.items():
        if target not in pins:
            continue
        _, digest, main, resolver = pins[target]
        cases.extend((
            f"        {platform})",
            f"            target='{target}'",
            f"            archive_sha='{digest}'",
            f"            main_sha='{main}'",
            f"            resolver_sha='{resolver}'",
            "            ;;",
        ))
    return "\n".join(cases)


def brew_sources(version, pins):
    lines = []
    for system in ("on_macos", "on_linux"):
        lines.append(f"  {system} do")
        if system == "on_linux" and OPTIONAL_TARGET not in pins:
            lines.append("    depends_on arch: :x86_64")
        for target, (_, target_system, architecture) in TARGETS.items():
            if target_system != system or target not in pins:
                continue
            name, digest, _, _ = pins[target]
            lines.extend((
                f"    {architecture} do",
                f'      url "{RELEASE}/{version}/{name}"',
                f'      sha256 "{digest}"',
                "    end",
            ))
        lines.append("  end")
    return "\n".join(lines)


def render(version, source, output):
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version):
        raise ValueError("Use a numeric version such as 0.1.6, without a v prefix.")
    pins = read_archives(version, source)
    output.mkdir()
    installer = output / "install.sh"
    installer.write_text(
        (TEMPLATES / "install.sh.in").read_text()
        .replace("@VERSION@", version)
        .replace("# @PIN_CASES@", shell_pins(pins))
    )
    installer.chmod(0o755)
    (output / "radio999k.rb").write_text(
        (TEMPLATES / "radio999k.rb.in").read_text()
        .replace("@VERSION@", version)
        .replace("# @BREW_SOURCES@", brew_sources(version, pins))
    )
    npm = output / "npm"
    shutil.copytree(TEMPLATES / "npm", npm)
    shutil.copy2(installer, npm / installer.name)
    shutil.copy2(TEMPLATES.parent / "LICENSE", npm / "LICENSE")
    manifest = npm / "package.json"
    package = json.loads(manifest.read_text())
    package["version"] = version
    del package["private"]
    manifest.write_text(json.dumps(package, indent=2) + "\n")


if __name__ == "__main__":
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("version")
    cli.add_argument("archive_directory", type=Path)
    cli.add_argument("output_directory", type=Path)
    args = cli.parse_args()
    try:
        render(args.version, args.archive_directory, args.output_directory)
    except (OSError, ValueError, tarfile.TarError) as error:
        cli.exit(1, f"999k: {error}\n")
