//! Optional in-process loader for trusted, first-party Rust dylib plugins.
//! The dynamic loader is available on Linux and macOS; other targets retain
//! the static host build without compiling platform-specific loader code.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use unix::*;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "loader"))]
mod resolver;
#[cfg(all(any(target_os = "linux", target_os = "macos"), feature = "loader"))]
pub use resolver::DylibResolver;
