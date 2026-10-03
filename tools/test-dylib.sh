#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_dir"
. tools/lib/dylib-common.sh
target_dir="${CARGO_TARGET_DIR:-$repo_dir/target}"
export CARGO_TARGET_DIR="$target_dir"
export RUTIS_SDK_LOCKFILE="$repo_dir/Cargo.lock"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$repo_dir=/src --remap-path-prefix=$target_dir=/target --remap-path-prefix=$cargo_home=/cargo"
build_all() {
  cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
    --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export
}
export RUTIS_SDK_ARTIFACT_SHA256="$(printf '0%.0s' {1..64})"
build_all
base="$(mktemp -d /tmp/rutis-dylib-smoke.XXXXXX)"
mkdir -p "$base/bad-boot"
cp "$target_dir/release/librutis_greeter_fixture_v1.$dylib_ext" "$base/bad-boot/libgreeter.$dylib_ext"
sdk_sha="$(sha256_of "$target_dir/release/librutis_sdk.$dylib_ext")"
export RUTIS_SDK_ARTIFACT_SHA256="$sdk_sha"
build_all
test "$(sha256_of "$target_dir/release/librutis_sdk.$dylib_ext")" = "$sdk_sha"

host="$target_dir/release/examples/greeter_host"
export "$loader_path_var=$target_dir/release:$(rustc --print target-libdir)"
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
    --sdk-file "$target_dir/release/librutis_sdk.$dylib_ext" \
    --output "$dir" \
    --prebuilt-library "$target_dir/release/librutis_greeter_fixture_$item.$dylib_ext"
done

# A third module changes id/name/injects; both swap and direct update must reject it.
cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
  --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export,rutis-greeter-fixture-v2/changed_identity
test "$(sha256_of "$target_dir/release/librutis_sdk.$dylib_ext")" = "$sdk_sha"
cargo xtask pack-plugin \
  --manifest-path "$repo_dir/tests/dylib-fixtures/greeter-v2/Cargo.toml" \
  --sdk-manifest "$base/sdk.toml" \
  --sdk-file "$target_dir/release/librutis_sdk.$dylib_ext" \
  --output "$base/changed-identity" \
  --prebuilt-library "$target_dir/release/librutis_greeter_fixture_v2.$dylib_ext"

# The first entry call fails; retry must reuse its mapped version slot.
cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
  --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export,rutis-greeter-fixture-v2/fail_once
test "$(sha256_of "$target_dir/release/librutis_sdk.$dylib_ext")" = "$sdk_sha"
cargo xtask pack-plugin \
  --manifest-path "$repo_dir/tests/dylib-fixtures/greeter-v2/Cargo.toml" \
  --sdk-manifest "$base/sdk.toml" \
  --sdk-file "$target_dir/release/librutis_sdk.$dylib_ext" \
  --output "$base/retry-entry" \
  --prebuilt-library "$target_dir/release/librutis_greeter_fixture_v2.$dylib_ext"

v1_hash="$(sha256_of "$base/v1/libgreeter.$dylib_ext")"
mkdir -p "$base/cache/$v1_hash"
printf truncated > "$base/cache/$v1_hash/libgreeter.$dylib_ext"
RUTIS_PLUGIN_CACHE="$base/cache" RUTIS_PLUGIN_DROP_MARKER="$base/plugin-drop-marker" "$host" "$base/v1" "$base/v2" "$base/changed-identity" "$base/retry-entry"
"$target_dir/release/examples/loader_host" "$base"
cp "$base/v1/plugin.toml" "$base/bad-boot/plugin.toml"
bad_sha="$(sha256_of "$base/bad-boot/libgreeter.$dylib_ext")"
sed_inplace "s/$(sha256_of "$base/v1/libgreeter.$dylib_ext")/$bad_sha/" "$base/bad-boot/plugin.toml"
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
sed_inplace "s/id = \"$sdk_id\"/id = \"$bad_id\"/" "$base/bad-l1/plugin.toml"
if RUTIS_PLUGIN_INIT_MARKER="$base/l1-init-marker" "$host" --load-only "$base/bad-l1" > "$base/l1.stdout" 2> "$base/l1.stderr"; then
  echo "mismatched SDK ID was accepted" >&2
  exit 1
fi
if test -e "$base/l1-init-marker" || ! grep -Fq 'SDK mismatch' "$base/l1.stderr"; then
  cat "$base/l1.stderr" >&2
  exit 1
fi
# The manifest must declare exactly the native libraries the binary links;
# the check runs before dlopen, so the initializer must not run either.
grep -Fq 'native_deps = [' "$base/v1/plugin.toml"
cp -a "$base/v1" "$base/bad-deps"
sed_inplace 's/^native_deps = .*/native_deps = []/' "$base/bad-deps/plugin.toml"
if RUTIS_PLUGIN_INIT_MARKER="$base/deps-init-marker" "$host" --load-only "$base/bad-deps" > "$base/deps.stdout" 2> "$base/deps.stderr"; then
  echo "undeclared native libraries were accepted" >&2
  exit 1
fi
if test -e "$base/deps-init-marker" || ! grep -Fq 'binary links native libraries' "$base/deps.stderr"; then
  cat "$base/deps.stderr" >&2
  exit 1
fi
# Plugins are built without a run path now that only the host and SDK set one.
if test -n "$(run_paths "$base/v1/libgreeter.$dylib_ext")"; then
  echo "plugin carries a run path" >&2
  exit 1
fi
if test "$dylib_os" = macos; then
  # Quarantine: the source is checked before it is read or cached, and the
  # cache entry before dlopen. dlopen of a quarantined file waits for
  # Gatekeeper, hence the timeout.
  quarantine() { xattr -w com.apple.quarantine "0081;00000000;rutis-test;" "$1"; }
  expect_quarantine_rejection() {
    local name="$1" cache="$2" dir="$3"
    if RUTIS_PLUGIN_CACHE="$cache" with_timeout 60 "$host" --load-only "$dir" > "$base/$name.stdout" 2> "$base/$name.stderr"; then
      echo "$name: quarantined plugin was accepted" >&2
      exit 1
    fi
    if ! grep -Fq 'com.apple.quarantine' "$base/$name.stderr"; then
      cat "$base/$name.stderr" >&2
      exit 1
    fi
  }
  cp -a "$base/v1" "$base/quarantined"
  quarantine "$base/quarantined/libgreeter.$dylib_ext"
  # 1. Quarantined source, empty cache: rejected, nothing written to the cache.
  expect_quarantine_rejection source-empty-cache "$base/q-cache-1" "$base/quarantined"
  if test -n "$(ls -A "$base/q-cache-1" 2> /dev/null)"; then
    echo "quarantined plugin reached the cache" >&2
    exit 1
  fi
  # 2. Quarantined source, clean cache entry with the same hash: still rejected.
  RUTIS_PLUGIN_CACHE="$base/q-cache-2" "$host" --load-only "$base/v1"
  expect_quarantine_rejection source-clean-cache "$base/q-cache-2" "$base/quarantined"
  # 3. Clean source, quarantined cache entry: rejected before dlopen.
  mkdir -p "$base/q-cache-3/$v1_hash"
  cp "$base/v1/libgreeter.$dylib_ext" "$base/q-cache-3/$v1_hash/"
  quarantine "$base/q-cache-3/$v1_hash/libgreeter.$dylib_ext"
  expect_quarantine_rejection clean-source-cache "$base/q-cache-3" "$base/v1"

  # A host signed with the hardened runtime ignores DYLD_* and validates
  # libraries; with disable-library-validation it loads ad-hoc signed ones.
  hardened="$target_dir/release/examples/greeter_host-hardened"
  cp "$host" "$hardened"
  cat > "$base/hardened.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.cs.disable-library-validation</key><true/></dict></plist>
PLIST
  codesign -f -s - -o runtime --entitlements "$base/hardened.plist" "$hardened"
  env -u DYLD_LIBRARY_PATH RUTIS_PLUGIN_CACHE="$base/hardened-cache" "$hardened" "$base/v1" "$base/v2"
  echo "hardened-runtime host swapped v1 to v2"
fi
echo "smoke artifacts: $base"
