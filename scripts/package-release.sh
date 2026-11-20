#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "Usage: $0 <target-triple> [output-dir]" >&2
  exit 2
fi

root=$(cd -- "$(dirname -- "$0")/.." && pwd)
target=$1
out=${2:-"$root/target/artifacts"}
case "$target" in
  aarch64-apple-darwin | x86_64-apple-darwin | aarch64-unknown-linux-gnu | x86_64-unknown-linux-gnu) ;;
  *) echo "Unsupported release target: $target" >&2; exit 2 ;;
esac
host=$(rustc -vV | sed -n 's/^host: //p')
if [[ "$target" != "$host" ]]; then
  echo "Release builds must be native: target $target, host $host" >&2
  exit 2
fi

mkdir -p -- "$out"
out=$(cd -- "$out" && pwd)
cd -- "$root"
cargo build --locked --release --target "$target" --target-dir target \
  --bin pigeonmq --bin pigeonmq-dot-resolve >&2

bin="target/$target/release"
banner=$("$bin/pigeonmq" --version)
if [[ ! "$banner" =~ ^pigeonmq\ [0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Expected a numbered release version from pigeonmq." >&2
  exit 1
fi
archive="pigeonmq-${banner#pigeonmq }-$target.tar.gz"
if [[ -e "$out/$archive" || -L "$out/$archive" ]]; then
  echo "Release archive already exists: $out/$archive" >&2
  exit 1
fi

stage=$(mktemp -d "$out/.package.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
install -m 0755 "$bin/pigeonmq" "$bin/pigeonmq-dot-resolve" "$stage"
install -m 0644 LICENSE "$stage/LICENSE"
COPYFILE_DISABLE=1 tar --format=ustar -C "$stage" -czf "$stage/$archive" \
  LICENSE pigeonmq pigeonmq-dot-resolve
ln -- "$stage/$archive" "$out/$archive"
printf '%s\n' "$out/$archive"
