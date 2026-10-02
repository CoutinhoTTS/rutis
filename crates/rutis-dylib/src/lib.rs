//! Optional in-process loader for trusted, first-party Rust dylib plugins.
//! The dynamic loader is available on Linux; other targets retain the static
//! host build without compiling platform-specific loader code.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(all(target_os = "linux", feature = "loader"))]
mod resolver;
#[cfg(all(target_os = "linux", feature = "loader"))]
pub use resolver::DylibResolver;
