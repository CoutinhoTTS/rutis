# Helpers shared by the dylib scripts so they run on Linux and macOS.
# Source this file; it works with the bash 3.2 that macOS ships.

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
  *)
    echo "dylib plugins are supported on Linux and macOS only" >&2
    exit 1
    ;;
esac

sha256_of() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# lib_name rutis-sdk -> librutis_sdk.so / librutis_sdk.dylib
lib_name() {
  local name="${1//-/_}"
  printf 'lib%s.%s' "$name" "$dylib_ext"
}

# The toolchain's dynamic libstd, which the bundle ships.
std_dylib() {
  local files
  files=("$(rustc --print target-libdir)"/libstd-*."$dylib_ext")
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

# Prints the run paths a library or executable carries, one per line.
run_paths() {
  if test "$dylib_os" = macos; then
    otool -l "$1" | awk '$1 == "cmd" { rpath = ($2 == "LC_RPATH") } rpath && $1 == "path" { print $2 }'
  else
    readelf -d "$1" | sed -n 's/.*(\(RUNPATH\|RPATH\)).*\[\(.*\)\]/\2/p'
  fi
}

# Prints the libraries a binary depends on, one per line.
needed_libs() {
  if test "$dylib_os" = macos; then
    otool -L "$1" | tail -n +2 | awk '{ print $1 }'
  else
    readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'
  fi
}

# Runs a command, killing it after the given number of seconds
# (macOS has no timeout(1)).
with_timeout() {
  local seconds="$1"
  shift
  perl -e 'alarm shift; exec @ARGV or die "exec: $!"' "$seconds" "$@"
}
