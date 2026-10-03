# Helpers shared by the dylib scripts so they run on Linux, macOS and
# Windows (Git Bash). Source this file; it works with the bash 3.2 that
# macOS ships.

dylib_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
exe_suffix=
case "$(uname -s)" in
  Linux)
    dylib_os=linux
    dylib_ext=so
    # The variable the example hosts use to find the SDK and libstd.
    loader_path_var=LD_LIBRARY_PATH
    ;;
  Darwin)
    dylib_os=macos
    dylib_ext=dylib
    loader_path_var=DYLD_LIBRARY_PATH
    # Written into every binary (LC_BUILD_VERSION), so it must not depend on
    # the machine (design-dylib-macos-windows §3.6).
    export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
    ;;
  MINGW* | MSYS* | CYGWIN*)
    dylib_os=windows
    dylib_ext=dll
    exe_suffix=.exe
    # Windows searches PATH for DLLs an executable's directory lacks.
    loader_path_var=PATH
    ;;
  *)
    echo "dylib plugins are supported on Linux, macOS and Windows only" >&2
    exit 1
    ;;
esac

# A path as native programs (cargo, rustc, the hosts) should receive it:
# C:/… on Windows, unchanged elsewhere.
native_path() {
  if test "$dylib_os" = windows; then
    cygpath -m "$1"
  else
    printf '%s' "$1"
  fi
}

# A path exactly as rustc records it, for --remap-path-prefix: C:\… on
# Windows, unchanged elsewhere.
remap_path() {
  if test "$dylib_os" = windows; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

# Prepends directories to the library search path of the example hosts.
add_library_path() {
  local dir joined=
  for dir in "$@"; do
    if test "$dylib_os" = windows; then
      dir="$(cygpath -u "$dir")"
    fi
    joined="${joined:+$joined:}$dir"
  done
  if test "$dylib_os" = windows; then
    export PATH="$joined:$PATH"
  else
    export "$loader_path_var=$joined"
  fi
}

sha256_of() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# lib_name rutis-sdk -> librutis_sdk.so / librutis_sdk.dylib / rutis_sdk.dll
lib_name() {
  local name="${1//-/_}"
  if test "$dylib_os" = windows; then
    printf '%s.dll' "$name"
  else
    printf 'lib%s.%s' "$name" "$dylib_ext"
  fi
}

# The toolchain's dynamic libstd, which the bundle ships.
std_dylib() {
  local files
  if test "$dylib_os" = windows; then
    # The same file is also in lib/rustlib/<target>/lib; bin is what rustup
    # puts on PATH.
    files=("$(cygpath -u "$(rustc --print sysroot)")"/bin/std-*.dll)
  else
    files=("$(rustc --print target-libdir)"/libstd-*."$dylib_ext")
  fi
  if test "${#files[@]}" -ne 1 || ! test -f "${files[0]}"; then
    echo "expected exactly one dynamic libstd in the toolchain" >&2
    return 1
  fi
  printf '%s' "${files[0]}"
}

sed_inplace() {
  if test "$dylib_os" = macos; then
    sed -i '' "$@"
  else
    sed -i "$@"
  fi
}

# Runs the repository's xtask, built once in its own target directory with
# the caller's RUSTFLAGS and CARGO_TARGET_DIR left out.
dylib_xtask() {
  env -u RUSTFLAGS -u CARGO_TARGET_DIR cargo run --quiet \
    --manifest-path "$(native_path "$dylib_repo/Cargo.toml")" \
    --target-dir "$(native_path "$dylib_repo/target/xtask")" \
    -p rutis-xtask -- "$@"
}

# Prints the run paths a library or executable carries, one per line.
# PE files have none.
run_paths() {
  if test "$dylib_os" = macos; then
    otool -l "$1" | awk '$1 == "cmd" { rpath = ($2 == "LC_RPATH") } rpath && $1 == "path" { print $2 }'
  elif test "$dylib_os" = linux; then
    readelf -d "$1" | sed -n 's/.*(\(RUNPATH\|RPATH\)).*\[\(.*\)\]/\2/p'
  fi
}

# Prints the libraries a binary depends on, one per line.
needed_libs() {
  if test "$dylib_os" = macos; then
    otool -L "$1" | tail -n +2 | awk '{ print $1 }'
  elif test "$dylib_os" = linux; then
    readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'
  else
    dylib_xtask inspect imports "$(native_path "$1")" | tr -d '\r'
  fi
}

# Runs a command, killing it after the given number of seconds
# (macOS has no timeout(1)).
with_timeout() {
  local seconds="$1"
  shift
  perl -e 'alarm shift; exec @ARGV or die "exec: $!"' "$seconds" "$@"
}
