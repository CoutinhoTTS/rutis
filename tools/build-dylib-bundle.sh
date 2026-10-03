#!/usr/bin/env bash
set -euo pipefail

# Build a Linux rutis-cli bundle whose public entry point is the verifier.
# Optional first argument selects a fresh output directory instead of the
# default content-addressed directory under target. Existing bundles are never overwritten.
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"
target_dir="${CARGO_TARGET_DIR:-$repo_dir/target}"
export CARGO_TARGET_DIR="$target_dir"
export RUTIS_SDK_LOCKFILE="$repo_dir/Cargo.lock"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$repo_dir=/src --remap-path-prefix=$target_dir=/target --remap-path-prefix=$cargo_home=/cargo"

# Resolve the SDK with the exact host feature graph. The first host binary is
# throwaway; the second pass binds the SDK artifact that graph produced.
export RUTIS_SDK_ARTIFACT_SHA256="$(printf '0%.0s' {1..64})"
cargo build --release -p rutis-cli --features dylib-plugins
sdk_file="$target_dir/release/librutis_sdk.so"
sdk_sha="$(sha256sum "$sdk_file" | cut -d ' ' -f 1)"

export RUTIS_SDK_ARTIFACT_SHA256="$sdk_sha"
cargo build --release -p rutis-cli --features dylib-plugins
test "$(sha256sum "$sdk_file" | cut -d ' ' -f 1)" = "$sdk_sha"

host_file="$target_dir/release/rutis-cli"
host_sha="$(sha256sum "$host_file" | cut -d ' ' -f 1)"
std_libdir="$(rustc --print target-libdir)"
std_files=("$std_libdir"/libstd-*.so)
test "${#std_files[@]}" -eq 1
std_file="${std_files[0]}"
std_name="$(basename "$std_file")"
std_sha="$(sha256sum "$std_file" | cut -d ' ' -f 1)"

export RUTIS_BUNDLE_HOST_FILE=rutis-cli-host
export RUTIS_BUNDLE_HOST_SHA256="$host_sha"
export RUTIS_BUNDLE_SDK_FILE=librutis_sdk.so
export RUTIS_BUNDLE_SDK_SHA256="$sdk_sha"
export RUTIS_BUNDLE_STD_FILE="$std_name"
export RUTIS_BUNDLE_STD_SHA256="$std_sha"
cargo build --release -p rutis-dylib-launcher

bundle="${1:-$target_dir/dylib-bundles/${host_sha:0:16}}"
if test -e "$bundle"; then
  echo "bundle already exists: $bundle" >&2
  exit 1
fi
mkdir -p "$bundle"
cp "$host_file" "$bundle/rutis-cli-host"
cp "$sdk_file" "$bundle/librutis_sdk.so"
cp "$std_file" "$bundle/$std_name"
cp "$target_dir/release/rutis-dylib-launcher" "$bundle/rutis-cli"
LD_LIBRARY_PATH="$bundle" "$bundle/rutis-cli-host" --sdk-info > "$bundle/sdk.toml"
cat >> "$bundle/sdk.toml" <<EOF

[build]
anchor_package = "rutis-cli"
anchor_features = ["dylib-plugins"]
EOF

# The launcher must not depend on either unchecked Rust dynamic library.
if ldd "$bundle/rutis-cli" | grep -Eq 'librutis_sdk|libstd-'; then
  echo "launcher dynamically links Rust SDK or libstd" >&2
  exit 1
fi
resolved="$(env -u LD_PRELOAD LD_LIBRARY_PATH="$bundle" ldd "$bundle/rutis-cli-host")"
if ! printf '%s\n' "$resolved" | grep -Fq "librutis_sdk.so => $bundle/librutis_sdk.so"; then
  echo "host SDK dependency resolved outside the bundle" >&2
  exit 1
fi
if ! printf '%s\n' "$resolved" | grep -Fq "$std_name => $bundle/$std_name"; then
  echo "host libstd dependency resolved outside the bundle" >&2
  exit 1
fi
echo "$bundle"
