#!/usr/bin/env bash
set -euo pipefail

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
sdk_name="$(lib_name rutis-sdk)"
plugin_name="$(lib_name greeter)"
build_all() {
  cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
    --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export
}
export RUTIS_SDK_ARTIFACT_SHA256="$(printf '0%.0s' {1..64})"
build_all
base="$(native_path "$(mktemp -d /tmp/rutis-dylib-smoke.XXXXXX)")"
mkdir -p "$base/bad-boot"
cp "$target_dir/release/$(lib_name rutis-greeter-fixture-v1)" "$base/bad-boot/$plugin_name"
sdk_sha="$(sha256_of "$target_dir/release/$sdk_name")"
export RUTIS_SDK_ARTIFACT_SHA256="$sdk_sha"
build_all
test "$(sha256_of "$target_dir/release/$sdk_name")" = "$sdk_sha"

host="$target_dir/release/examples/greeter_host"
add_library_path "$target_dir/release" "$(rustc --print target-libdir)"
sdk_info="$("$host" --sdk-info | tr -d '\r')"
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
    --sdk-file "$target_dir/release/$sdk_name" \
    --output "$dir" \
    --prebuilt-library "$target_dir/release/$(lib_name rutis-greeter-fixture-$item)"
done

# A third module changes id/name/injects; both swap and direct update must reject it.
cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
  --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export,rutis-greeter-fixture-v2/changed_identity
test "$(sha256_of "$target_dir/release/$sdk_name")" = "$sdk_sha"
cargo xtask pack-plugin \
  --manifest-path "$repo_dir/tests/dylib-fixtures/greeter-v2/Cargo.toml" \
  --sdk-manifest "$base/sdk.toml" \
  --sdk-file "$target_dir/release/$sdk_name" \
  --output "$base/changed-identity" \
  --prebuilt-library "$target_dir/release/$(lib_name rutis-greeter-fixture-v2)"

# The first entry call fails; retry must reuse its mapped version slot.
cargo build --release -p rutis-dylib -p rutis-greeter-fixture-v1 -p rutis-greeter-fixture-v2 \
  --lib --examples --features rutis-dylib/loader,rutis-greeter-fixture-v1/export,rutis-greeter-fixture-v2/export,rutis-greeter-fixture-v2/fail_once
test "$(sha256_of "$target_dir/release/$sdk_name")" = "$sdk_sha"
cargo xtask pack-plugin \
  --manifest-path "$repo_dir/tests/dylib-fixtures/greeter-v2/Cargo.toml" \
  --sdk-manifest "$base/sdk.toml" \
  --sdk-file "$target_dir/release/$sdk_name" \
  --output "$base/retry-entry" \
  --prebuilt-library "$target_dir/release/$(lib_name rutis-greeter-fixture-v2)"

v1_hash="$(sha256_of "$base/v1/$plugin_name")"
mkdir -p "$base/cache/$v1_hash"
printf truncated > "$base/cache/$v1_hash/$plugin_name"
RUTIS_PLUGIN_CACHE="$base/cache" RUTIS_PLUGIN_DROP_MARKER="$base/plugin-drop-marker" "$host" "$base/v1" "$base/v2" "$base/changed-identity" "$base/retry-entry"
"$target_dir/release/examples/loader_host" "$base"
cp "$base/v1/plugin.toml" "$base/bad-boot/plugin.toml"
bad_sha="$(sha256_of "$base/bad-boot/$plugin_name")"
sed_inplace "s/$(sha256_of "$base/v1/$plugin_name")/$bad_sha/" "$base/bad-boot/plugin.toml"
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
if test -n "$(run_paths "$base/v1/$plugin_name")"; then
  echo "plugin carries a run path" >&2
  exit 1
fi
if test "$dylib_os" = windows; then
  # A plugin is opened with LOAD_LIBRARY_SEARCH_APPLICATION_DIR |
  # LOAD_LIBRARY_SEARCH_SYSTEM32, so DLLs next to it in the cache are never
  # searched; it reuses the SDK and libstd the host loaded. Plant a valid but
  # wrong DLL under both names in the cache entry: picking either up would
  # fail the load.
  planted="$base/planted-cache/$v1_hash"
  mkdir -p "$planted"
  cp "$base/v1/$plugin_name" "$planted/"
  std_name="$(basename "$(std_dylib)")"
  for name in "$sdk_name" "$std_name"; do
    cp "$(cygpath -u "$SYSTEMROOT")/System32/version.dll" "$planted/$name"
  done
  # The fixture's .CRT$XCU initializer must run on a successful load, or the
  # "initializer did not run" checks above would prove nothing.
  RUTIS_PLUGIN_CACHE="$base/planted-cache" RUTIS_PLUGIN_INIT_MARKER="$base/positive-init-marker" \
    "$host" --load-only "$base/v1"
  test -e "$base/positive-init-marker"
  echo "DLLs planted next to the cached plugin were not used; the plugin initializer ran"
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
  quarantine "$base/quarantined/$plugin_name"
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
  cp "$base/v1/$plugin_name" "$base/q-cache-3/$v1_hash/"
  quarantine "$base/q-cache-3/$v1_hash/$plugin_name"
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
