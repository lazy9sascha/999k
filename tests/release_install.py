#!/usr/bin/env python3
"""Offline smoke tests for the three installers using the same fixture archives."""

import hashlib
import io
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile

REPO = Path(__file__).resolve().parents[1]
VERSION = "0.1.6"
TARGETS = {
    ("Darwin", "arm64"): "aarch64-apple-darwin",
    ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
    ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
}


def run(*args, env=None, code=0):
    result = subprocess.run(args, env=env, text=True, capture_output=True, check=False)
    assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
    return result


def archive(directory, target):
    path = directory / f"999k-{VERSION}-{target}.tar.gz"
    files = {
        "999k": f'#!/bin/sh\nprintf "%s\\n" "{target}:$1"\n',
        "999k-dot-resolve": '#!/bin/sh\nprintf "%s\\n" "resolver:$1"\n',
        "LICENSE": "Fixture license\n",
    }
    with tarfile.open(path, "w:gz") as tar:
        for name, text in files.items():
            data = text.encode()
            member = tarfile.TarInfo(name)
            member.size = len(data)
            member.mode = 0o755 if name != "LICENSE" else 0o644
            tar.addfile(member, io.BytesIO(data))
    return f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}"


def write_command(path, body):
    path.write_text("#!/bin/sh\nset -eu\n" + body)
    path.chmod(0o755)


def smoke(root):
    target = TARGETS[(platform.system(), platform.machine())]
    release, scripts, home, temp = [root / directory for directory in ("release", "scripts", "home", "temp")]
    for directory in release, scripts, home, temp:
        directory.mkdir()
    assets = {triple: archive(release, triple) for triple in TARGETS.values()}
    manifest = release / "SHA256SUMS"
    manifest.write_text("\n".join(assets.values()) + "\n")
    good_manifest = manifest.read_bytes()
    name = assets[target].split("  ")[1]
    fixture = release / name
    good_archive = fixture.read_bytes()

    write_command(scripts / "curl", """
while [ "$#" -gt 1 ]; do
    if [ "$1" = -o ]; then out=$2; shift; fi
    shift
done
case $1 in
    https://github.com/lazy9sascha/999k/releases/download/0.1.6/999k-0.1.6-*.tar.gz) ;;
    *) exit 99 ;;
esac
printf '%s\\n' "${1##*/}" >> "$FIXTURE_DOWNLOADS"
cp "$FIXTURE_RELEASE/${1##*/}" "$out"
if [ -n "${FIXTURE_COMPETING_FILE:-}" ]; then
    printf '%s\n' 'other release' > "$FIXTURE_COMPETING_FILE"
fi
""")
    downloads = root / "downloads"
    env = dict(os.environ, PATH=f"{scripts}:{os.environ['PATH']}", HOME=str(home), TMPDIR=str(temp),
               FIXTURE_RELEASE=str(release), FIXTURE_DOWNLOADS=str(downloads),
               npm_config_cache=str(root / "npm-cache"), npm_config_userconfig=str(root / "npmrc"))
    generate = (sys.executable, REPO / "packaging/render.py", VERSION, release)
    output = root / "output"
    run(*generate, output, env=env)
    installer = output / "install.sh"
    assert installer.read_bytes() == (output / "npm/install.sh").read_bytes()
    formula = (output / "radio999k.rb").read_text()
    for pins in assets.values():
        digest, filename = pins.split("  ")
        assert f'/{VERSION}/{filename}"\n      sha256 "{digest}"' in formula
    if shutil.which("ruby"):
        run("ruby", "-c", output / "radio999k.rb", env=env)

    # Default and quoted custom install directories. Bad bytes must not replace a good pair.
    legacy = home / ".local/bin"
    legacy.mkdir(parents=True)
    write_command(legacy / "pigeonmq", "exit 91\n")
    write_command(legacy / "pigeonmq-dot-resolve", "exit 92\n")
    run("sh", installer, env=env)
    assert downloads.read_text().splitlines() == [name]
    assert run(home / ".local/bin/999k", "--version", env=env).stdout.strip() == f"{target}:--version"
    run(legacy / "pigeonmq", env=env, code=91)
    run(legacy / "pigeonmq-dot-resolve", env=env, code=92)
    custom = home / "custom bin"
    run("sh", installer, "--bin-dir", custom, env=env)
    fetched = downloads.read_bytes()
    run("sh", installer, "--bin-dir", custom, env=env)
    assert downloads.read_bytes() == fetched
    old = home / "old"
    old.mkdir()
    write_command(old / "999k", "exit 91\n")
    failed = run("sh", installer, "--bin-dir", old, env=env, code=1)
    assert "Existing file differs" in failed.stderr
    assert run(old / "999k", env=env, code=91).stdout == ""
    (old / "999k").unlink()
    (old / "999k-dot-resolve").symlink_to(custom / "999k-dot-resolve")
    failed = run("sh", installer, "--bin-dir", old, env=env, code=1)
    assert "Cannot replace non-regular file" in failed.stderr
    assert downloads.read_bytes() == fetched
    raced = home / "competing install"
    foreign = raced / "999k-dot-resolve"
    failed = run("sh", installer, "--bin-dir", raced,
                 env=dict(env, FIXTURE_COMPETING_FILE=str(foreign)), code=1)
    assert "Existing file differs" in failed.stderr
    assert foreign.read_bytes() == b"other release\n"
    assert sorted(path.name for path in raced.iterdir()) == ["999k-dot-resolve"]

    fixture.write_bytes(good_archive + b"corrupt")
    bad = home / "bad download"
    failed = run("sh", installer, "--bin-dir", bad, env=env, code=1)
    assert "SHA-256 mismatch" in failed.stderr
    assert list(bad.iterdir()) == []
    assert run(custom / "999k-dot-resolve", "quoted value", env=env).stdout.strip() == "resolver:quoted value"
    rejected = root / "rejected"
    run(*generate, rejected, env=env, code=1)
    assert not rejected.exists()
    fixture.write_bytes(good_archive)

    # npm packs the embedded installer and resolves its bin links without registry or lifecycle scripts.
    pack = run("npm", "pack", "--offline", "--ignore-scripts", "--pack-destination", root,
               output / "npm", env=env).stdout.strip().splitlines()[-1]
    bundled = ("npm", "exec", "--offline", "--ignore-scripts", "--yes", f"--package={root / pack}", "--")
    assert run(*bundled, "999k", "two words", env=env).stdout.strip() == f"{target}:two words"
    fetched = downloads.read_bytes()
    assert run(*bundled, "999k-dot-resolve", "warm", env=env).stdout.strip() == "resolver:warm"
    assert downloads.read_bytes() == fetched
    assert run("sh", installer, "--run", "pigeonmq", "legacy", env=env).stdout.strip() == f"{target}:legacy"
    assert downloads.read_bytes() == fetched
    cached = home / f".local/share/999k/npm/{VERSION}-{target}/bin/999k"
    write_command(cached, "exit 91\n")
    fixture.write_bytes(good_archive + b"corrupt")
    failed = run(*bundled, "999k", "--version", env=env, code=1)
    assert "SHA-256 mismatch" in failed.stderr
    fixture.write_bytes(good_archive)
    assert run(*bundled, "999k", "--version", env=env).stdout.strip() == f"{target}:--version"

    # Intel Mac must stop before it downloads a binary for a different platform.
    write_command(scripts / "uname", 'case $1 in -s) echo Darwin;; -m) echo x86_64;; esac\n')
    fetched = downloads.read_bytes()
    failed = run("sh", installer, env=env, code=1)
    assert "No 999K" in failed.stderr
    assert downloads.read_bytes() == fetched
    assert "depends_on arch: :arm64" in formula and "x86_64-apple-darwin" not in formula

    manifest.write_text("\n".join(value for key, value in assets.items()
                                 if key != "aarch64-unknown-linux-gnu") + "\n")
    failed = run(*generate, rejected, env=env, code=1)
    assert "Missing release targets: aarch64-unknown-linux-gnu" in failed.stderr
    assert not rejected.exists()

    manifest.write_bytes(good_manifest + good_manifest.splitlines()[0] + b"\n")
    run(*generate, rejected, env=env, code=1)
    assert not rejected.exists()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="999k-install-test.") as directory:
        smoke(Path(directory))
    print("Direct and npm installer smoke tests, pinned archives, and formula checks passed.")
