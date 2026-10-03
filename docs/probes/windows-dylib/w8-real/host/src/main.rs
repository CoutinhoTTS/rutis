//! W8 with the real SDK: load the greeter fixture copy with LoadLibraryExW,
//! check its metadata, build the factory and run the plugin on a rutis Ctx.
//! usage: w8-real-host <greeter.dll> <drop-marker-file>

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    use rutis_sdk::rutis::{CordisError, Ctx, PluginFactory, Snapshot};
    use rutis_sdk::{ConfigValue, PluginMeta};
    use std::ffi::CString;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
        LOAD_LIBRARY_SEARCH_SYSTEM32,
    };

    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(90));
        println!("RESULT W8.real: FAIL timeout");
        std::process::exit(3);
    });

    let args: Vec<String> = std::env::args().collect();
    let plugin = PathBuf::from(&args[1]);
    let marker = PathBuf::from(&args[2]);
    std::env::set_var("RUTIS_PLUGIN_DROP_MARKER", &marker);

    let wide: Vec<u16> = plugin.as_os_str().encode_wide().chain(Some(0)).collect();
    let h = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    assert!(!h.is_null(), "load failed: {}", unsafe { GetLastError() });
    let sym = |name: &str| unsafe {
        let c = CString::new(name).unwrap();
        GetProcAddress(h, c.as_ptr() as *const u8).unwrap_or_else(|| panic!("no {name}"))
    };
    type Entry = fn() -> Result<Box<dyn PluginFactory<ConfigValue>>, CordisError>;
    let meta: fn() -> PluginMeta = unsafe { std::mem::transmute(sym("rutis_plugin_meta")) };
    let entry: Entry = unsafe { std::mem::transmute(sym("rutis_plugin_entry")) };

    let meta = meta();
    let meta_ok = meta.sdk_id == rutis_sdk::SDK_ID && meta.id == "greeter";
    println!("RESULT W8.real.meta: {} plugin sdk_id={} host SDK_ID={} id={} version={}",
        if meta_ok { "PASS" } else { "FAIL" }, meta.sdk_id, rutis_sdk::SDK_ID, meta.id, meta.version);

    let rt = rutis_sdk::tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let outcome: Result<(Option<String>, Option<u64>), String> = rt.block_on(async move {
        let factory = entry().map_err(|e| e.to_string())?;
        let plugin = factory.build(&ConfigValue::Null).map_err(|e| e.to_string())?;
        let root = Ctx::root().map_err(|e| e.to_string())?;
        let view = root.plugin(plugin);
        (&view).await.map_err(|e| e.to_string())?;
        let text = root.get::<String>().map(|s| (*s).clone());
        let generation = root.get::<Snapshot>().map(|s| s.generation);
        view.dispose().await.map_err(|e| e.to_string())?;
        root.shutdown().await.map_err(|e| e.to_string())?;
        drop(factory);
        Ok((text, generation))
    });
    let marker_text = std::fs::read_to_string(&marker).unwrap_or_default();
    match outcome {
        Ok((text, generation)) => {
            let ok = text.as_deref() == Some("hello v1") && generation == Some(1) && marker_text == "drop\n";
            println!(
                "RESULT W8.real: {} provided={text:?} snapshot_generation={generation:?} drop_marker={marker_text:?}",
                if ok && meta_ok { "PASS" } else { "FAIL" }
            );
            std::process::exit(if ok && meta_ok { 0 } else { 1 });
        }
        Err(e) => {
            println!("RESULT W8.real: FAIL {e}");
            std::process::exit(1);
        }
    }
}
