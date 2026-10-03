//! Probe SDK: shared types, a shared counter and a thread-local that host and
//! plugins must see as one instance.

use std::cell::Cell;
use std::sync::atomic::AtomicUsize;

pub use tokio;

#[global_allocator]
static SDK_ALLOCATOR: std::alloc::System = std::alloc::System;

/// A type both sides name; its TypeId must be equal in host and plugin.
#[derive(Debug, PartialEq)]
pub struct Shared(pub u32);

/// The trait object a plugin hands to the host.
pub trait Greeter: Send + Sync {
    fn greet(&self) -> String;
    fn version(&self) -> &'static str;
}

pub static COUNTER: AtomicUsize = AtomicUsize::new(0);
pub static DROPS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// const-initialized thread-local.
    pub static TL: Cell<u64> = const { Cell::new(0) };
    /// lazily initialized thread-local (different codegen path).
    pub static TL_LAZY: Cell<u64> = Cell::new(std::hint::black_box(0));
}

/// A marker that differs between the real SDK and the planted copy in W3.
#[inline(never)]
pub fn mark() -> &'static str {
    match option_env!("SDK_MARK") {
        Some(mark) => mark,
        None => "good",
    }
}

/// Address of this thread's `TL`, computed inside the SDK.
#[inline(never)]
pub fn tl_addr() -> usize {
    TL.with(|c| c as *const Cell<u64> as usize)
}
