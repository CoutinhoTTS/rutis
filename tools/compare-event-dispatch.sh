#!/usr/bin/env bash
# Compare both implementations in one release binary with identical dependencies.
set -euo pipefail
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
baseline="${1:-603f8220b049c6ad26e82f258fd72f214b8575fe}"
base="$(mktemp -d /tmp/rutis-event-compare.XXXXXX)"
mkdir -p "$base/baseline" "$base/probe/src"
git -C "$repo_dir" archive "$baseline" | tar -C "$base/baseline" -xf -
cp "$repo_dir/Cargo.lock" "$base/probe/Cargo.lock"
cp "$repo_dir/docs/probes/compare-exact-dispatch.rs" "$base/probe/src/main.rs"
python3 - "$repo_dir" "$base" <<'PY'
import json
import pathlib
import sys
repo, base = map(pathlib.Path, sys.argv[1:])
manifest = f'''[package]
name = "rutis-events-compare"
version = "0.0.0"
edition = "2021"

[dependencies]
old = {{ package = "rutis", path = {json.dumps(str(base / "baseline/crates/rutis"))} }}
new = {{ package = "rutis", path = {json.dumps(str(repo / "crates/rutis"))} }}
tokio = {{ version = "1", features = ["rt-multi-thread", "sync", "time", "macros"] }}
'''
(base / "probe/Cargo.toml").write_text(manifest)
PY
CARGO_TARGET_DIR="$base/target" cargo build --release --offline --manifest-path "$base/probe/Cargo.toml"
if test -n "${RUTIS_BENCH_CPU:-}"; then
  taskset -c "$RUTIS_BENCH_CPU" "$base/target/release/rutis-events-compare"
else
  "$base/target/release/rutis-events-compare"
fi
echo "comparison artifacts: $base"
