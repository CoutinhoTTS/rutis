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
test "$(cd /tmp && "$bundle/rutis-cli" --version | tr -d '\r')" = "rutis-cli 0.6.0"
sdk_name="$(lib_name rutis-sdk)"
host_name="rutis-cli-host$exe_suffix"
if test "$dylib_os" = windows; then
  std_name="$(cd "$bundle" && ls std-*.dll)"
else
  std_name="$(cd "$bundle" && ls libstd-*."$dylib_ext")"
fi

# The host's exit code reaches the caller (exec on Unix, forwarded on Windows).
set +e
"$bundle/rutis-cli" --no-such-option > /dev/null 2>&1
code=$?
set -e
if test "$code" -ne 2; then
  echo "launcher returned $code for a host that exited with 2" >&2
  exit 1
fi

if test "$dylib_os" = windows; then
  # Windows has no loader variables to clear. A copy of the SDK or libstd
  # in the working directory or on PATH must not be used: the host's
  # directory is searched first, and the launcher has checked that both
  # files are there. The planted files are valid DLLs without the SDK's
  # exports, so a host that picked one up would fail to start.
  planted="$base/planted"
  mkdir -p "$planted"
  for name in "$sdk_name" "$std_name"; do
    cp "$(cygpath -u "$SYSTEMROOT")/System32/version.dll" "$planted/$name"
  done
  (cd "$planted" && "$bundle/rutis-cli" --sdk-info > /dev/null)
  PATH="$planted:$PATH" "$bundle/rutis-cli" --sdk-info > /dev/null
  echo "copies of the SDK and libstd in the working directory and on PATH were not used"
else
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
fi

for name in "$host_name" "$sdk_name" "$std_name"; do
  copy="$base/modified-$name"
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
  copy="$base/missing-$name"
  cp -a "$bundle" "$copy"
  rm "$copy/$name"
  if "$copy/rutis-cli" --version > "$copy/stdout" 2> "$copy/stderr"; then
    echo "launcher accepted a bundle without $name" >&2
    exit 1
  fi
  if test -s "$copy/stdout" || ! grep -Fq 'rutis dylib bundle rejected' "$copy/stderr"; then
    echo "launcher ran host or reported the wrong rejection for missing $name" >&2
    exit 1
  fi
done
echo "launcher rejected modified or missing host, SDK and libstd before execution"

if test "$dylib_os" = windows; then
  # While the host runs, the launcher holds the three files with read-only
  # sharing. rutis-cli has no long-running mode without a terminal, so bind
  # a second launcher to a copy of PING.EXE as the host, next to the same
  # SDK and libstd files.
  hold="$base/hold"
  mkdir -p "$hold"
  cp "$(cygpath -u "$SYSTEMROOT")/System32/PING.EXE" "$hold/$host_name"
  cp "$bundle/$sdk_name" "$bundle/$std_name" "$hold/"
  (
    export RUTIS_BUNDLE_HOST_FILE="$host_name"
    export RUTIS_BUNDLE_HOST_SHA256="$(sha256_of "$hold/$host_name")"
    export RUTIS_BUNDLE_SDK_FILE="$sdk_name"
    export RUTIS_BUNDLE_SDK_SHA256="$(sha256_of "$hold/$sdk_name")"
    export RUTIS_BUNDLE_STD_FILE="$std_name"
    export RUTIS_BUNDLE_STD_SHA256="$(sha256_of "$hold/$std_name")"
    cargo build --release -p rutis-dylib-launcher --target-dir "$(native_path "$base/hold-target")"
  )
  cp "$base/hold-target/release/rutis-dylib-launcher.exe" "$hold/rutis-cli.exe"
  host_running() {
    tasklist //FI "IMAGENAME eq $host_name" //NH | grep -Fqi "$host_name"
  }
  if host_running; then
    echo "a $host_name process is already running; cannot test the job object" >&2
    exit 1
  fi
  "$hold/rutis-cli.exe" -n 60 127.0.0.1 > /dev/null &
  launcher_pid=$!
  for _ in $(seq 1 50); do
    host_running && break
    sleep 0.2
  done
  host_running
  for name in "$host_name" "$sdk_name" "$std_name"; do
    before="$(sha256_of "$hold/$name")"
    (printf x >> "$hold/$name") 2> /dev/null || true
    rm -f "$hold/$name" 2> /dev/null || true
    mv "$hold/$name" "$hold/$name.moved" 2> /dev/null || true
    if ! test -f "$hold/$name" || test -e "$hold/$name.moved" \
      || test "$(sha256_of "$hold/$name")" != "$before"; then
      echo "$name could be modified, deleted or renamed while the host ran" >&2
      exit 1
    fi
  done
  echo "host, SDK and libstd could not be modified, deleted or renamed while the host ran"
  # Killing the launcher closes its job object, which ends the host.
  taskkill //F //PID "$(cat "/proc/$launcher_pid/winpid")" > /dev/null
  wait "$launcher_pid" 2> /dev/null || true
  for _ in $(seq 1 50); do
    host_running || break
    sleep 0.2
  done
  if host_running; then
    echo "host kept running after the launcher was killed" >&2
    exit 1
  fi
  echo "killing the launcher ended the host"
fi
