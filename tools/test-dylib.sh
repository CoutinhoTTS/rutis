#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"
target_dir="${CARGO_TARGET_DIR:-$repo_dir/target}"
export CARGO_TARGET_DIR="$target_dir"
export RUTIS_SDK_LOCKFILE="$repo_dir/Cargo.lock"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$repo_dir=/src --remap-path-prefix=$target_dir=/target --remap-path-prefix=$cargo_home=/cargo -C link-arg=-Wl,-rpath,\$ORIGIN"
build_all() {
  cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
    --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export
}
export RUTIS_SDK_ARTIFACT_SHA256="$(printf '0%.0s' {1..64})"
build_all
base="$(mktemp -d /tmp/rutis-dylib-smoke.XXXXXX)"
mkdir -p "$base/bad-boot"
cp "$target_dir/release/librutis_greeter_fixture_v1.so" "$base/bad-boot/libgreeter.so"
sdk_sha="$(sha256sum "$target_dir/release/librutis_sdk.so" | cut -d ' ' -f 1)"
export RUTIS_SDK_ARTIFACT_SHA256="$sdk_sha"
build_all
test "$(sha256sum "$target_dir/release/librutis_sdk.so" | cut -d ' ' -f 1)" = "$sdk_sha"

host="$target_dir/release/examples/greeter_host"
export LD_LIBRARY_PATH="$target_dir/release:$(rustc --print target-libdir)"
sdk_info="$("$host" --sdk-info)"
sdk_id="${sdk_info%% *}"
sdk_version="${sdk_info#* }"
target="$(rustc -vV | sed -n 's/^host: //p')"
rustc_version="$(rustc --version)"
cat > "$base/sdk.toml" <<EOF
[sdk]
version = "$sdk_version"
id = "$sdk_id"
artifact_sha256 = "$sdk_sha"
target = "$target"
rustc = "$rustc_version"
EOF

for item in v1 v2; do
  dir="$base/$item"
  cargo xtask pack-plugin \
    --manifest-path "$repo_dir/tests/dylib-fixtures/greeter-$item/Cargo.toml" \
    --sdk-manifest "$base/sdk.toml" \
    --sdk-file "$target_dir/release/librutis_sdk.so" \
    --output "$dir" \
    --prebuilt-library "$target_dir/release/librutis_greeter_fixture_$item.so"
done

# A third module changes id/name/injects; both swap and direct update must reject it.
cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
  --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export,rutis-greeter-fixture-v2/changed_identity
test "$(sha256sum "$target_dir/release/librutis_sdk.so" | cut -d ' ' -f 1)" = "$sdk_sha"
cargo xtask pack-plugin \
  --manifest-path "$repo_dir/tests/dylib-fixtures/greeter-v2/Cargo.toml" \
  --sdk-manifest "$base/sdk.toml" \
  --sdk-file "$target_dir/release/librutis_sdk.so" \
  --output "$base/changed-identity" \
  --prebuilt-library "$target_dir/release/librutis_greeter_fixture_v2.so"

# The first entry call fails; retry must reuse its mapped version slot.
cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
  --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export,rutis-greeter-fixture-v2/fail_once
test "$(sha256sum "$target_dir/release/librutis_sdk.so" | cut -d ' ' -f 1)" = "$sdk_sha"
cargo xtask pack-plugin \
  --manifest-path "$repo_dir/tests/dylib-fixtures/greeter-v2/Cargo.toml" \
  --sdk-manifest "$base/sdk.toml" \
  --sdk-file "$target_dir/release/librutis_sdk.so" \
  --output "$base/retry-entry" \
  --prebuilt-library "$target_dir/release/librutis_greeter_fixture_v2.so"

v1_hash="$(sha256sum "$base/v1/libgreeter.so" | cut -d ' ' -f 1)"
mkdir -p "$base/cache/$v1_hash"
printf truncated > "$base/cache/$v1_hash/libgreeter.so"
RUTIS_PLUGIN_CACHE="$base/cache" RUTIS_PLUGIN_DROP_MARKER="$base/plugin-drop-marker" "$host" "$base/v1" "$base/v2" "$base/changed-identity" "$base/retry-entry"
"$target_dir/release/examples/loader_host" "$base"
cp "$base/v1/plugin.toml" "$base/bad-boot/plugin.toml"
bad_sha="$(sha256sum "$base/bad-boot/libgreeter.so" | cut -d ' ' -f 1)"
sed -i "s/$(sha256sum "$base/v1/libgreeter.so" | cut -d ' ' -f 1)/$bad_sha/" "$base/bad-boot/plugin.toml"
marker="$base/plugin-init-marker"
if RUTIS_PLUGIN_INIT_MARKER="$marker" "$host" --load-only "$base/bad-boot" > "$base/rejected.stdout" 2> "$base/rejected.stderr"; then
  echo "mismatched embedded SDK artifact was accepted" >&2
  exit 1
fi
if test -e "$marker"; then
  echo "plugin initializer ran before embedded identity rejection" >&2
  exit 1
fi
if ! grep -Fq 'binary identity differs from manifest or host' "$base/rejected.stderr"; then
  cat "$base/rejected.stderr" >&2
  exit 1
fi
cp -a "$base/v1" "$base/bad-l1"
bad_id="$(printf 'f%.0s' {1..64})"
sed -i "s/id = \"$sdk_id\"/id = \"$bad_id\"/" "$base/bad-l1/plugin.toml"
if RUTIS_PLUGIN_INIT_MARKER="$base/l1-init-marker" "$host" --load-only "$base/bad-l1" > "$base/l1.stdout" 2> "$base/l1.stderr"; then
  echo "mismatched SDK ID was accepted" >&2
  exit 1
fi
if test -e "$base/l1-init-marker" || ! grep -Fq 'SDK mismatch' "$base/l1.stderr"; then
  cat "$base/l1.stderr" >&2
  exit 1
fi
echo "smoke artifacts: $base"
