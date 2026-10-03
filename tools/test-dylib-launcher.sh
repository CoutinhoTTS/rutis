#!/usr/bin/env bash
set -euo pipefail

# Physical paths: rustc sees symlinks resolved (macOS /tmp is /private/tmp),
# and --remap-path-prefix only matches the path rustc sees.
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo_dir"
. tools/lib/dylib-common.sh
base="$(mktemp -d /tmp/rutis-dylib-launcher.XXXXXX)"
# Keep test output outside the cached target tree so repeated runs cannot
# collide with an immutable bundle restored from a previous build.
bundle="${1:-$(bash tools/build-dylib-bundle.sh "$base/bundle" | tail -n 1)}"
test -d "$bundle"
test "$(cd /tmp && "$bundle/rutis-cli" --version)" = "rutis-cli 0.6.0"
env "$loader_path_var=/tmp" "$bundle/rutis-cli" --sdk-info > /dev/null

# A library search path pointing at another SDK must not reach the host.
# The impostor is a valid library that lacks the SDK's symbols: a host that
# picked it up aborts (the command fails under set -e), and its initializer
# leaves a marker if it ran.
mkdir -p "$base/hostile"
cat > "$base/marker.c" <<'C'
#include <stdio.h>
#include <stdlib.h>
#ifdef __APPLE__
#define PROGRAM getprogname()
#else
extern char *program_invocation_short_name;
#define PROGRAM program_invocation_short_name
#endif
__attribute__((constructor)) static void mark(void) {
    const char *path = getenv("RUTIS_TEST_MARKER");
    FILE *file = path ? fopen(path, "a") : NULL;
    if (file) {
        fprintf(file, "%s\n", PROGRAM);
        fclose(file);
    }
}
C
if test "$dylib_os" = macos; then
  cc -dynamiclib -o "$base/hostile/librutis_sdk.dylib" "$base/marker.c"
else
  cc -shared -fPIC -o "$base/hostile/librutis_sdk.so" "$base/marker.c"
fi
cp -a "$bundle" "$base/relocated"
env "$loader_path_var=$base/hostile" RUTIS_TEST_MARKER="$base/search-path-marker" \
  "$base/relocated/rutis-cli" --sdk-info > /dev/null
if test -e "$base/search-path-marker"; then
  echo "host loaded an SDK from the caller's $loader_path_var" >&2
  exit 1
fi

# Injected libraries may run in the launcher itself (outside its protection,
# SDK design §5.4), but must not be passed on to the host.
if test "$dylib_os" = macos; then
  inject_var=DYLD_INSERT_LIBRARIES
else
  inject_var=LD_PRELOAD
fi
env "$inject_var=$base/hostile/$(lib_name rutis-sdk)" RUTIS_TEST_MARKER="$base/inject-marker" \
  "$bundle/rutis-cli" --sdk-info > /dev/null
if test -e "$base/inject-marker" && grep -Fxq rutis-cli-host "$base/inject-marker"; then
  echo "$inject_var reached the host" >&2
  exit 1
fi

std_name="$(cd "$bundle" && ls libstd-*."$dylib_ext")"
for name in rutis-cli-host "$(lib_name rutis-sdk)" "$std_name"; do
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
