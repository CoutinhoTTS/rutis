//! Optional in-process loader for trusted, first-party Rust dylib plugins.
//!
//! The dynamic loader is available on Linux, macOS and Windows (x64, MSVC);
//! other targets retain the static host build without compiling
//! platform-specific loader code. The checks before a library is opened, the
//! cache, retention and the plugin factory are shared (`loader`); each
//! platform module only opens libraries, finds symbols and locates the loaded
//! SDK. See docs/dylib-sdk-implementation.md for platform differences.
//!
//! On Windows, plugins are opened with `LoadLibraryExW` by full path and
//! `LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`, and the
//! cached file is held with read-only sharing from the hash check until it is
//! mapped. Static initializers of a plugin (`.CRT$XCU`, e.g. the `ctor`
//! crate) run under the loader lock and must not wait for other threads.

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod loader;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use loader::*;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(all(
    any(target_os = "linux", target_os = "macos", windows),
    feature = "loader"
))]
mod resolver;
#[cfg(all(
    any(target_os = "linux", target_os = "macos", windows),
    feature = "loader"
))]
pub use resolver::DylibResolver;
