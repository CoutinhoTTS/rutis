#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"
base="$(mktemp -d /tmp/rutis-dylib-launcher.XXXXXX)"
# Keep test output outside the cached target tree so repeated runs cannot
# collide with an immutable bundle restored from a previous build.
bundle="${1:-$(bash tools/build-dylib-bundle.sh "$base/bundle" | tail -n 1)}"
test -d "$bundle"
test "$(cd /tmp && "$bundle/rutis-cli" --version)" = "rutis-cli 0.6.0"
env LD_LIBRARY_PATH=/tmp "$bundle/rutis-cli" --sdk-info > /dev/null

mkdir -p "$base/hostile"
printf 'not an SDK' > "$base/hostile/librutis_sdk.so"
cp -a "$bundle" "$base/relocated"
env LD_LIBRARY_PATH="$base/hostile" "$base/relocated/rutis-cli" --sdk-info > /dev/null
for name in rutis-cli-host librutis_sdk.so "$(find "$bundle" -maxdepth 1 -name 'libstd-*.so' -printf '%f\n')"; do
  copy="$base/$(basename "$name")"
  cp -a "$bundle" "$copy"
  printf x >> "$copy/$name"
  if "$copy/rutis-cli" --version > "$copy/stdout" 2> "$copy/stderr"; then
    echo "launcher accepted modified $name" >&2
    exit 1
  fi
  if test -s "$copy/stdout" || ! grep -Fq 'SHA-256 mismatch' "$copy/stderr"; then
    echo "launcher ran host or reported the wrong rejection for $name" >&2
    exit 1
  fi
done
echo "launcher rejected modified host, SDK and libstd before execution"
