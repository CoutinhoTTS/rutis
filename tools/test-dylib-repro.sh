#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$repo_dir/tools/lib/dylib-common.sh"
base="$(mktemp -d /tmp/rutis-dylib-repro.XXXXXX)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
for slot in a b; do
  source_dir="$base/$slot/source"
  target_dir="$base/$slot/target"
  mkdir -p "$source_dir"
  tar -C "$repo_dir" --exclude=./target --exclude=./.git -cf - . | tar -C "$source_dir" -xf -
  export CARGO_TARGET_DIR="$target_dir"
  export RUTIS_SDK_LOCKFILE="$source_dir/Cargo.lock"
  export RUSTFLAGS="--remap-path-prefix=$source_dir=/src --remap-path-prefix=$target_dir=/target --remap-path-prefix=$cargo_home=/cargo"
  # Build the SDK through the host anchor, as tools/build-dylib-bundle.sh does.
  # Built as the primary package, rutis-sdk would statically link std and
  # produce a different artifact from the one the host links.
  export RUTIS_SDK_ARTIFACT_SHA256="$(printf '0%.0s' {1..64})"
  cargo build --release --locked --offline -p rutis-cli --features dylib-plugins --manifest-path "$source_dir/Cargo.toml"
  sdk_file="$target_dir/release/$(lib_name rutis-sdk)"
  if ! needed_libs "$sdk_file" | grep -Eq "(^|/)libstd-[^/]*\.$dylib_ext\$"; then
    needed_libs "$sdk_file" >&2
    echo "SDK does not depend on the dynamic libstd" >&2
    exit 1
  fi
  if test "$dylib_os" = macos && dsymutil -s "$sdk_file" | grep -q 'N_OSO'; then
    echo "SDK records object file paths (N_OSO debug map); it cannot be reproducible" >&2
    exit 1
  fi
  sha256_of "$sdk_file" > "$base/$slot.sha"
done
cmp "$base/a.sha" "$base/b.sha"
echo "two independent source and target paths produced the same SDK artifact: $(cat "$base/a.sha")"
echo "SDK depends on the dynamic libstd"
# The classification checks only exercise rutis-sdk's build.rs, so checking
# rutis-sdk directly is enough.
base_flags="$RUSTFLAGS"
export RUSTFLAGS="$base_flags -D warnings"
cargo check --release --locked --offline -p rutis-sdk --manifest-path "$base/b/source/Cargo.toml" > /dev/null
echo "lint-only RUSTFLAGS argument was accepted"
export RUSTFLAGS="$base_flags -C relocation-model=pic"
if cargo check --release --locked --offline -p rutis-sdk --manifest-path "$base/b/source/Cargo.toml" > "$base/unclassified.stdout" 2> "$base/unclassified.stderr"; then
  echo "unclassified RUSTFLAGS argument was accepted" >&2
  exit 1
fi
if ! grep -Fq 'unclassified RUSTFLAGS codegen option: relocation-model' "$base/unclassified.stderr"; then
  cat "$base/unclassified.stderr" >&2
  exit 1
fi
echo "unclassified RUSTFLAGS argument was rejected"
