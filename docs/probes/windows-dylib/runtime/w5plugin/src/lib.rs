//! W5: a plugin with a static initializer (`ctor`, which places a pointer in
//! `.CRT$XCU`; the CRT runs it from DllMain, under the loader lock) and a
//! thread-local with a destructor.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

static CTOR_RAN: AtomicBool = AtomicBool::new(false);
static CTOR_THREAD: Mutex<String> = Mutex::new(String::new());
static TLS_DTORS: AtomicUsize = AtomicUsize::new(0);
static TLS_INITS: AtomicUsize = AtomicUsize::new(0);

/// What typical registration-style initializers do: allocate, lock a mutex,
/// touch thread-locals (including one in the SDK), read the environment.
#[ctor::ctor]
unsafe fn init() {
    let buf: Vec<u8> = vec![1; 4096];
    let _ = std::hint::black_box(buf);
    sdk::TL.with(|c| c.set(c.get() + 1000));
    LOCAL.with(|_| {});
    let _ = std::env::var_os("PATH");
    let _ = std::time::Instant::now();
    *CTOR_THREAD.lock().unwrap() = format!("{:?}", std::thread::current().id());
    sdk::COUNTER.fetch_add(1, Ordering::SeqCst);
    CTOR_RAN.store(true, Ordering::SeqCst);
}

struct Guard;

impl Guard {
    fn new() -> Self {
        TLS_INITS.fetch_add(1, Ordering::SeqCst);
        Guard
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        TLS_DTORS.fetch_add(1, Ordering::SeqCst);
    }
}

thread_local! {
    static LOCAL: Guard = Guard::new();
}

#[no_mangle]
pub fn w5_ctor_info() -> (bool, String) {
    (CTOR_RAN.load(Ordering::SeqCst), CTOR_THREAD.lock().unwrap().clone())
}

/// Touch the destructor-bearing thread-local on the calling thread.
#[no_mangle]
pub fn w5_touch() {
    LOCAL.with(|_| {});
}

#[no_mangle]
pub fn w5_tls_counts() -> (usize, usize) {
    (TLS_INITS.load(Ordering::SeqCst), TLS_DTORS.load(Ordering::SeqCst))
}
