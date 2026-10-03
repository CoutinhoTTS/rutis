//! Probe plugin. Built twice with GREETER_VERSION=1 and 2 (W2); also carries
//! the boot blob for W6.

use sdk::tokio;
use sdk::{Greeter, Shared};
use std::any::{Any, TypeId};
use std::sync::atomic::Ordering;

const VERSION: &str = match option_env!("GREETER_VERSION") {
    Some(v) => v,
    None => "1",
};

/// W6: the boot blob, in a section with a name of at most 8 bytes.
#[used]
#[link_section = ".rutism"]
static RUTIS_BOOT_META: [u8; 512] = blob();

/// W6 side check: the section name the Linux macro uses today (longer than 8 bytes).
#[used]
#[link_section = ".note.rutis.meta"]
static RUTIS_BOOT_META_LONG: [u8; 64] = *b"RUTIS_PROBE_LONG_SECTION_NAME_0123456789abcdef0123456789abcdef!!";

const fn blob() -> [u8; 512] {
    let mut out = [0u8; 512];
    let magic = b"RUTIS_PROBE_BOOT";
    let mut i = 0;
    while i < 512 {
        out[i] = if i < magic.len() { magic[i] } else { ((i * 31 + 7) % 251) as u8 };
        i += 1;
    }
    out
}

/// A plugin-private type: only the plugin knows its layout and Drop.
struct Hello {
    n: u32,
    text: String,
}

impl Greeter for Hello {
    fn greet(&self) -> String {
        format!("{} #{} from greeter v{VERSION}", self.text, self.n)
    }
    fn version(&self) -> &'static str {
        VERSION
    }
}

impl Drop for Hello {
    fn drop(&mut self) {
        sdk::DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[no_mangle]
pub fn greeter_version() -> &'static str {
    VERSION
}

#[no_mangle]
pub fn greeter_make() -> Box<dyn Greeter> {
    Box::new(Hello { n: 7, text: String::from("hello") })
}

#[no_mangle]
pub fn greeter_type_id() -> TypeId {
    TypeId::of::<Shared>()
}

#[no_mangle]
pub fn greeter_any() -> Box<dyn Any + Send> {
    Box::new(Shared(42))
}

/// Downcast a host-made value inside the plugin.
#[no_mangle]
pub fn greeter_downcast(value: Box<dyn Any + Send>) -> Option<u32> {
    value.downcast::<Shared>().ok().map(|s| s.0)
}

/// Returns false if there is no current tokio runtime. Otherwise spawns two
/// tasks (via Handle and via tokio::spawn) that report their thread name.
#[no_mangle]
pub fn greeter_spawn(tx: tokio::sync::mpsc::UnboundedSender<String>) -> bool {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    let tx2 = tx.clone();
    handle.spawn(async move {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let name = std::thread::current().name().unwrap_or("?").to_string();
        let _ = tx.send(format!("handle.spawn on {name}"));
    });
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        let name = std::thread::current().name().unwrap_or("?").to_string();
        let _ = tx2.send(format!("tokio::spawn on {name}"));
    });
    true
}

#[no_mangle]
pub fn greeter_tl_read() -> (u64, u64) {
    (sdk::TL.with(|c| c.get()), sdk::TL_LAZY.with(|c| c.get()))
}

#[no_mangle]
pub fn greeter_tl_write(v: u64) {
    sdk::TL.with(|c| c.set(v));
    sdk::TL_LAZY.with(|c| c.set(v + 1));
}

/// Address of TL computed in plugin code (inlined access) and via the SDK.
#[no_mangle]
pub fn greeter_tl_addr() -> (usize, usize) {
    (sdk::TL.with(|c| c as *const _ as usize), sdk::tl_addr())
}

#[no_mangle]
pub fn greeter_counter_bump() -> usize {
    sdk::COUNTER.fetch_add(1, Ordering::SeqCst) + 1
}

#[no_mangle]
pub fn greeter_sdk_mark() -> &'static str {
    sdk::mark()
}

#[no_mangle]
pub fn greeter_panic() {
    panic!("boom from plugin v{VERSION}");
}
