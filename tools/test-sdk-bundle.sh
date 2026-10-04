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
echo "[sdk-bundle-test] building the runtime bundle"
bash tools/build-dylib-bundle.sh "$base/runtime"
sdk_sha="$(sed -n 's/^artifact_sha256 = "\(.*\)"/\1/p' "$base/runtime/sdk.toml")"
test "$(sha256_of "$base/runtime/$sdk_name")" = "$sdk_sha"

# The sdk-bundle: prebuilt SDK, closure rlibs (shrunk by the probe), manifest.
echo "[sdk-bundle-test] packing the sdk-bundle"
with_timeout 2400 cargo xtask pack-sdk-bundle --bundle-dir "$base/runtime" --output "$base/sdk-bundle"
test "$(sha256_of "$base/sdk-bundle/lib/$sdk_name")" = "$sdk_sha"
test -f "$base/sdk-bundle/bundle.toml"
test -f "$base/sdk-bundle/GUIDE.md"
test -f "$base/sdk-bundle/cargo-config.toml"

# A plugin workspace with no host sources and no rutis-sdk dependency. The
# fixture sources work unchanged: `use rutis_sdk::` resolves through the
# injected --extern.
for item in v1 v2; do
  echo "[sdk-bundle-test] packaging external plugin $item"
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
  with_timeout 900 env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin \
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
  echo "[sdk-bundle-test] loading $item on the published host"
  if ! with_timeout 600 env RUTIS_PLUGIN_INIT_MARKER="$marker" \
      "$base/runtime/rutis-cli" --scripted --load-only \
      --plugin "$base/$item" --plugin-config '{}' > /dev/null; then
    echo "the published host rejected or hung on $item" >&2
    exit 1
  fi
done
# Only the v1 fixture writes the initializer marker; the v2 fixture has
# none, so v2 is covered by the exit code above.
test -f "$base/init-v1"

# The plugin links the SDK and libstd dynamically and carries no run path.
imports="$(cargo xtask inspect imports "$base/v1/$plugin_name")"
case "$imports" in *"$sdk_name"*) ;; *) echo "plugin does not link $sdk_name" >&2; exit 1;; esac
# Windows names it std-<hash>.dll (no lib prefix); Linux and macOS libstd-.
case "$imports" in
  *libstd-*|*std-[0-9a-f][0-9a-f]*) ;;
  *) echo "plugin does not link dynamic libstd" >&2; exit 1;;
esac
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
if with_timeout 900 env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin --manifest-path "$base/bad-manifest/Cargo.toml" \
  --bundle "$base/sdk-bundle" --output "$base/bad-out" > "$base/e3.stdout" 2>&1; then
  echo "a direct rutis-sdk dependency was accepted" >&2
  exit 1
fi
grep -Fq 'rutis-sdk (dependencies)' "$base/e3.stdout"

# E6: a modified closure file is rejected against the bundle manifest.
cp -a "$base/sdk-bundle" "$base/tampered"
first_rlib="$(ls "$base/tampered/deps"/*.rlib | head -1)"
printf 'x' >> "$first_rlib"
if with_timeout 900 env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin --manifest-path "$base/external/plugin-v1/Cargo.toml" \
  --bundle "$base/tampered" --features export --output "$base/bad-out" \
  > "$base/e6.stdout" 2>&1; then
  echo "a modified bundle file was accepted" >&2
  exit 1
fi
grep -Fq 'differs from the bundle manifest' "$base/e6.stdout"

# E9: a missing closure file names the file.
rm -f "$first_rlib"
if with_timeout 900 env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin --manifest-path "$base/external/plugin-v1/Cargo.toml" \
  --bundle "$base/tampered" --features export --output "$base/bad-out" \
  > "$base/e9.stdout" 2>&1; then
  echo "a missing bundle file was accepted" >&2
  exit 1
fi
grep -Fq "the bundle is missing" "$base/e9.stdout"

# E2a: a private dependency outside the SDK tree builds and the published
# host loads the plugin.
e2a="$base/external/e2a-plugin"
mkdir -p "$e2a/src"
cp rust-toolchain.toml "$e2a/rust-toolchain.toml"
cat > "$e2a/Cargo.toml" <<'EOF'
[package]
name = "e2a-plugin"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["dylib"]
test = false
doctest = false

[features]
export = []

[dependencies]
base64 = "0.22"

[workspace]
EOF
cat > "$e2a/src/lib.rs" <<'EOF'
#![cfg(feature = "export")]
use rutis_sdk::rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, PluginFactory};
use rutis_sdk::ConfigValue;
use base64::Engine as _;

struct Factory;
struct E2a;
impl PluginFactory<ConfigValue> for Factory {
    fn name(&self) -> &str { "e2a" }
    fn build(&self, _: &ConfigValue) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(E2a))
    }
}
impl Plugin for E2a {
    fn name(&self) -> &str { "e2a" }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let encoded = base64::engine::general_purpose::STANDARD.encode("hello e2a");
            ctx.provide(encoded)?;
            Ok(Effect::Done)
        })
    }
}
rutis_sdk::export_plugin! { id: "e2a", factory: Factory }
EOF
echo "[sdk-bundle-test] E2a: private dependency outside the SDK tree"
with_timeout 900 env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin \
  --manifest-path "$e2a/Cargo.toml" --bundle "$base/sdk-bundle" \
  --features export --output "$base/e2a"
with_timeout 600 "$base/runtime/rutis-cli" --scripted --load-only --plugin "$base/e2a" --plugin-config '{}' > /dev/null

# E2d: a private dependency that pulls tokio into the plugin graph collides
# with the SDK closure; the packer must reject it readably.
e2d="$base/external/e2d-plugin"
mkdir -p "$e2d/src"
cp rust-toolchain.toml "$e2d/rust-toolchain.toml"
cat > "$e2d/Cargo.toml" <<'EOF'
[package]
name = "e2d-plugin"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["dylib"]
test = false
doctest = false

[features]
export = []

[dependencies]
tokio-stream = "0.1"

[workspace]
EOF
cat > "$e2d/src/lib.rs" <<'EOF'
#![cfg(feature = "export")]
use rutis_sdk::rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, PluginFactory};
use rutis_sdk::ConfigValue;
// Actually touching the private dependency's API loads its tokio copy's
// metadata, which is what collides with the SDK closure; merely declaring
// the dependency does not.
use tokio_stream::StreamExt as _;

struct Factory;
struct E2d;
impl PluginFactory<ConfigValue> for Factory {
    fn name(&self) -> &str { "e2d" }
    fn build(&self, _: &ConfigValue) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(E2d))
    }
}
impl Plugin for E2d {
    fn name(&self) -> &str { "e2d" }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let _ = tokio_stream::iter(vec![1u8]).next();
            ctx.provide("e2d".to_string())?;
            Ok(Effect::Done)
        })
    }
}
rutis_sdk::export_plugin! { id: "e2d", factory: Factory }
EOF
echo "[sdk-bundle-test] E2d: private dependency overlapping the closure"
if with_timeout 900 env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS cargo xtask pack-plugin \
  --manifest-path "$e2d/Cargo.toml" --bundle "$base/sdk-bundle" \
  --features export --output "$base/e2d" > "$base/e2d.stdout" 2>&1; then
  echo "a private dependency overlapping the SDK closure was accepted" >&2
  exit 1
fi
# rustc's failure shape for the overlap is not stable: colliding
# StableCrateId when the crate-id clashes, or a misleading E0463 naming the
# private crate. Either way the build must fail, and the error must point
# at the overlapping crates, not at an unrelated plugin bug.
grep -Eq 'colliding StableCrateId|E0463|E0277' "$base/e2d.stdout" || {
  cat "$base/e2d.stdout" >&2
  echo "unexpected failure mode for the closure overlap" >&2
  exit 1
}
grep -Eq 'tokio_stream|tokio-stream|pin_project' "$base/e2d.stdout" || {
  cat "$base/e2d.stdout" >&2
  echo "the overlap failure does not name the overlapping crates" >&2
  exit 1
}

echo "sdk-bundle external build passed"
