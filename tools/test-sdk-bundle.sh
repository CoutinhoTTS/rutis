#!/usr/bin/env bash
set -euo pipefail

# External plugin builds against a prebuilt SDK (design-sdk-build-package
# §六, E1/E3/E6/E9): produce an sdk-bundle from a runtime bundle, build a
# plugin in a standalone workspace with no rutis-sdk dependency, swap v1→v2
# on the example host, and assert the packer's rejection paths.

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
sdk_name="$(lib_name rutis-sdk)"
plugin_name="$(lib_name greeter)"

base="$(native_path "$(mktemp -d /tmp/rutis-sdk-bundle.XXXXXX)")"

# The runtime bundle (host, SDK, libstd, launcher) the sdk-bundle belongs to.
bash tools/build-dylib-bundle.sh "$base/runtime"
sdk_sha="$(sed -n 's/^artifact_sha256 = "\(.*\)"/\1/p' "$base/runtime/sdk.toml")"
test "$(sha256_of "$base/runtime/$sdk_name")" = "$sdk_sha"

# The sdk-bundle: prebuilt SDK, closure rlibs (shrunk by the probe), manifest.
cargo xtask pack-sdk-bundle --bundle-dir "$base/runtime" --output "$base/sdk-bundle"
test "$(sha256_of "$base/sdk-bundle/lib/$sdk_name")" = "$sdk_sha"
test -f "$base/sdk-bundle/bundle.toml"
test -f "$base/sdk-bundle/GUIDE.md"
test -f "$base/sdk-bundle/cargo-config.toml"

# A plugin workspace with no host sources and no rutis-sdk dependency. The
# fixture sources work unchanged: `use rutis_sdk::` resolves through the
# injected --extern.
for item in v1 v2; do
  workspace="$base/external/plugin-$item"
  mkdir -p "$workspace/src"
  cp "tests/dylib-fixtures/greeter-$item/src/lib.rs" "$workspace/src/lib.rs"
  cp rust-toolchain.toml "$workspace/rust-toolchain.toml"
  cat > "$workspace/Cargo.toml" <<EOF
[package]
name = "external-greeter-$item"
version = "${item#v}.0.0"
edition = "2021"
publish = false

[lib]
crate-type = ["dylib"]
test = false
doctest = false

[features]
export = []

[workspace]
EOF
  env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin \
    --manifest-path "$workspace/Cargo.toml" \
    --bundle "$base/sdk-bundle" \
    --features export \
    --output "$base/$item"
done

# The published host loads both packaged plugins (v1 then v2 in separate
# runs; the swap path itself is Loader code shared with the anchor mode and
# is covered by tools/test-dylib.sh — what this test proves is that the
# externally built binaries pass the host's identity, dependency and
# lifecycle checks: the plugin initializer runs, meaning dlopen succeeded
# with all pre-load checks).
for item in v1 v2; do
  marker="$base/init-$item"
  RUTIS_PLUGIN_INIT_MARKER="$marker" "$base/runtime/rutis-cli" --scripted \
    --plugin "$base/$item" --plugin-config '{}' > /dev/null
  test -f "$marker"
done

# The plugin links the SDK and libstd dynamically and carries no run path.
imports="$(cargo xtask inspect imports "$base/v1/$plugin_name")"
case "$imports" in *"$sdk_name"*) ;; *) echo "plugin does not link $sdk_name" >&2; exit 1;; esac
case "$imports" in *"libstd-"*) ;; *) echo "plugin does not link dynamic libstd" >&2; exit 1;; esac
if test -n "$(run_paths "$base/v1/$plugin_name")"; then
  echo "external plugin carries a run path" >&2
  exit 1
fi

# E3: a direct rutis-sdk dependency is rejected before anything is built.
mkdir -p "$base/bad-manifest/src"
printf 'fn main() {}\n' > "$base/bad-manifest/src/lib.rs"
cat > "$base/bad-manifest/Cargo.toml" <<'EOF'
[package]
name = "bad-manifest"
version = "0.1.0"
edition = "2021"

[dependencies]
rutis-sdk = "0.5"
EOF
if env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin --manifest-path "$base/bad-manifest/Cargo.toml" \
  --bundle "$base/sdk-bundle" --output "$base/bad-out" > "$base/e3.stdout" 2>&1; then
  echo "a direct rutis-sdk dependency was accepted" >&2
  exit 1
fi
grep -Fq 'rutis-sdk (dependencies)' "$base/e3.stdout"

# E6: a modified closure file is rejected against the bundle manifest.
cp -a "$base/sdk-bundle" "$base/tampered"
first_rlib="$(ls "$base/tampered/deps"/*.rlib | head -1)"
printf 'x' >> "$first_rlib"
if env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin --manifest-path "$base/external/plugin-v1/Cargo.toml" \
  --bundle "$base/tampered" --features export --output "$base/bad-out" \
  > "$base/e6.stdout" 2>&1; then
  echo "a modified bundle file was accepted" >&2
  exit 1
fi
grep -Fq 'differs from the bundle manifest' "$base/e6.stdout"

# E9: a missing closure file names the file.
rm -f "$first_rlib"
if env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin --manifest-path "$base/external/plugin-v1/Cargo.toml" \
  --bundle "$base/tampered" --features export --output "$base/bad-out" \
  > "$base/e9.stdout" 2>&1; then
  echo "a missing bundle file was accepted" >&2
  exit 1
fi
grep -Fq "the bundle is missing" "$base/e9.stdout"

echo "sdk-bundle external build passed"
