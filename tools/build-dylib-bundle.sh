#!/usr/bin/env bash
set -euo pipefail

# Build a Linux, macOS or Windows rutis-cli bundle whose public entry point
# is the verifier. Optional first argument selects a fresh output directory
# instead of the default content-addressed directory under target. Existing
# bundles are never overwritten.
# Physical paths: rustc sees symlinks resolved (macOS /tmp is /private/tmp),
# and --remap-path-prefix only matches the path rustc sees.
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo_dir"
. tools/lib/dylib-common.sh
target_dir="${CARGO_TARGET_DIR:-$repo_dir/target}"
mkdir -p "$target_dir"
target_dir="$(cd "$target_dir" && pwd -P)"
export CARGO_TARGET_DIR="$(native_path "$target_dir")"
export RUTIS_SDK_LOCKFILE="$(native_path "$repo_dir/Cargo.lock")"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
cargo_home="$(cd "$cargo_home" 2> /dev/null && pwd -P || printf '%s' "$cargo_home")"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$(remap_path "$repo_dir")=/src --remap-path-prefix=$(remap_path "$target_dir")=/target --remap-path-prefix=$(remap_path "$cargo_home")=/cargo"

# Resolve the SDK with the exact host feature graph. The first host binary is
# throwaway; the second pass binds the SDK artifact that graph produced.
sdk_name="$(lib_name rutis-sdk)"
export RUTIS_SDK_ARTIFACT_SHA256="$(printf '0%.0s' {1..64})"
cargo build --release -p rutis-cli --features dylib-plugins
sdk_file="$target_dir/release/$sdk_name"
sdk_sha="$(sha256_of "$sdk_file")"

export RUTIS_SDK_ARTIFACT_SHA256="$sdk_sha"
cargo build --release -p rutis-cli --features dylib-plugins
test "$(sha256_of "$sdk_file")" = "$sdk_sha"

host_file="$target_dir/release/rutis-cli$exe_suffix"
host_sha="$(sha256_of "$host_file")"
std_file="$(std_dylib)"
std_name="$(basename "$std_file")"
std_sha="$(sha256_of "$std_file")"

export RUTIS_BUNDLE_HOST_FILE="rutis-cli-host$exe_suffix"
export RUTIS_BUNDLE_HOST_SHA256="$host_sha"
export RUTIS_BUNDLE_SDK_FILE="$sdk_name"
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
# Only these files: on Windows the import library (rutis_sdk.dll.lib) and
# debug files next to the build outputs are not part of the bundle.
cp "$host_file" "$bundle/rutis-cli-host$exe_suffix"
cp "$sdk_file" "$bundle/$sdk_name"
cp "$std_file" "$bundle/$std_name"
cp "$target_dir/release/rutis-dylib-launcher$exe_suffix" "$bundle/rutis-cli$exe_suffix"
if test "$dylib_os" = windows; then
  # The host's imports resolve from its own directory.
  "$bundle/rutis-cli-host$exe_suffix" --sdk-info | tr -d '\r' > "$bundle/sdk.toml"
else
  env "$loader_path_var=$bundle" "$bundle/rutis-cli-host" --sdk-info > "$bundle/sdk.toml"
fi
# The lock and the toolchain pin must not become bundle files: the macOS
# launcher tests codesign-verify everything in the directory. Record their
# hashes instead; cargo xtask pack-sdk-bundle takes the files from the
# repository checkout and refuses a mismatch.
cat >> "$bundle/sdk.toml" <<EOF

[build]
anchor_package = "rutis-cli"
anchor_features = ["dylib-plugins"]
lock_sha256 = "$(sha256_of "$repo_dir/Cargo.lock")"
toolchain_sha256 = "$(sha256_of "$repo_dir/rust-toolchain.toml")"
EOF

# The launcher must not depend on either unchecked Rust dynamic library.
if needed_libs "$bundle/rutis-cli$exe_suffix" | grep -Eiq '(^|/)(librutis_sdk|libstd-|rutis_sdk|std-)'; then
  echo "launcher dynamically links Rust SDK or libstd" >&2
  exit 1
fi
if test "$dylib_os" = macos; then
  # dyld finds the SDK and libstd through the host's run path, which must be
  # the bundle directory only; the SDK must be known by its @rpath name.
  host_deps="$(needed_libs "$bundle/rutis-cli-host")"
  for dep in "@rpath/librutis_sdk.dylib" "@rpath/$std_name"; do
    if ! printf '%s\n' "$host_deps" | grep -Fxq "$dep"; then
      echo "host does not link $dep" >&2
      exit 1
    fi
  done
  if test "$(run_paths "$bundle/rutis-cli-host")" != "@loader_path"; then
    echo "host run paths are not exactly @loader_path: $(run_paths "$bundle/rutis-cli-host")" >&2
    exit 1
  fi
  if test "$(otool -D "$bundle/librutis_sdk.dylib" | tail -n 1)" != "@rpath/librutis_sdk.dylib"; then
    echo "SDK install name is not @rpath/librutis_sdk.dylib" >&2
    exit 1
  fi
  for file in "$bundle"/*; do
    case "$file" in *.toml) continue ;; esac
    codesign --verify "$file"
  done
elif test "$dylib_os" = windows; then
  # The host imports the SDK and libstd by these exact names; Windows finds
  # them in the host's directory before any other.
  host_deps="$(needed_libs "$bundle/rutis-cli-host.exe")"
  for dep in "$sdk_name" "$std_name"; do
    if ! printf '%s\n' "$host_deps" | grep -Fixq "$dep"; then
      echo "host does not import $dep: $host_deps" >&2
      exit 1
    fi
  done
  # A DLL can export at most 65535 symbols. The release SDK exported 1597
  # in the feasibility test; fail long before the limit (SDK design §十一).
  exports="$(dylib_xtask inspect export-count "$(native_path "$bundle/$sdk_name")" | tr -d '\r')"
  echo "$sdk_name exports $exports symbols (limit 65535, CI threshold 30000)" >&2
  if test "$exports" -gt 30000; then
    echo "$sdk_name exports more than 30000 symbols" >&2
    exit 1
  fi
else
  resolved="$(env -u LD_PRELOAD LD_LIBRARY_PATH="$bundle" ldd "$bundle/rutis-cli-host")"
  if ! printf '%s\n' "$resolved" | grep -Fq "librutis_sdk.so => $bundle/librutis_sdk.so"; then
    echo "host SDK dependency resolved outside the bundle" >&2
    exit 1
  fi
  if ! printf '%s\n' "$resolved" | grep -Fq "$std_name => $bundle/$std_name"; then
    echo "host libstd dependency resolved outside the bundle" >&2
    exit 1
  fi
fi
echo "$bundle"
