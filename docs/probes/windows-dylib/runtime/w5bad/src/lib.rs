//! W5 negative control: a static initializer that starts a thread and waits
//! for it. Under the loader lock the new thread cannot start running until
//! DllMain returns, so the wait is expected to time out (a join would hang).

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// 0 = not run, 1 = the thread answered in time, 2 = the wait timed out.
static OUTCOME: AtomicU8 = AtomicU8::new(0);

#[ctor::ctor]
unsafe fn init() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(());
    });
    let outcome = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => 1,
        Err(_) => 2,
    };
    OUTCOME.store(outcome, Ordering::SeqCst);
}

#[no_mangle]
pub fn w5bad_outcome() -> u8 {
    OUTCOME.load(Ordering::SeqCst)
}
