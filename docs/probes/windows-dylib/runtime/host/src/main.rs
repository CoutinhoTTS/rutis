//! Probe host. Links the probe SDK (static import of sdk.dll and std-*.dll)
//! and loads plugins with LoadLibraryExW by full path.
//!
//! host w8 <plugin.dll>
//! host w2 <plugin-a.dll> <plugin-b.dll>
//! host w3 <plugin.dll>
//! host w5 <w5plugin.dll>
//! host w5bad <w5bad.dll>
//! host w7 <plugin.dll> <workdir>

#[cfg(not(windows))]
fn main() {
    eprintln!("this probe host only runs on Windows");
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(windows)]
mod imp {
    use sdk::tokio;
    use sdk::{Greeter, Shared};
    use std::any::{Any, TypeId};
    use std::ffi::CString;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{GetLastError, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
        LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    use windows_sys::Win32::System::ProcessStatus::EnumProcessModules;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn load(path: &Path) -> Result<HMODULE, u32> {
        let w = wide(path);
        let h = unsafe {
            LoadLibraryExW(
                w.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if h.is_null() {
            Err(unsafe { GetLastError() })
        } else {
            Ok(h)
        }
    }

    /// Look up a `#[no_mangle] pub fn` and reinterpret it as the Rust fn type `T`.
    unsafe fn sym<T: Copy>(h: HMODULE, name: &str) -> T {
        let c = CString::new(name).unwrap();
        let p = GetProcAddress(h, c.as_ptr() as *const u8)
            .unwrap_or_else(|| panic!("symbol {name} not found"));
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of_val(&p));
        std::mem::transmute_copy(&p)
    }

    fn module_path(h: HMODULE) -> String {
        let mut buf = vec![0u16; 4096];
        let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) };
        String::from_utf16_lossy(&buf[..n as usize])
    }

    /// Loaded modules whose file name looks like ours (sdk, std, greeter, w5*).
    fn our_modules() -> Vec<String> {
        let mut mods: Vec<HMODULE> = vec![std::ptr::null_mut(); 1024];
        let mut needed = 0u32;
        let ok = unsafe {
            EnumProcessModules(
                GetCurrentProcess(),
                mods.as_mut_ptr(),
                (mods.len() * std::mem::size_of::<HMODULE>()) as u32,
                &mut needed,
            )
        };
        assert!(ok != 0, "EnumProcessModules failed");
        let count = needed as usize / std::mem::size_of::<HMODULE>();
        mods[..count.min(mods.len())]
            .iter()
            .map(|h| module_path(*h))
            .filter(|p| {
                let name = Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                name == "sdk.dll"
                    || name.starts_with("std-")
                    || name.starts_with("greeter")
                    || name.starts_with("w5")
                    || name.starts_with("vcruntime")
            })
            .collect()
    }

    fn print_modules(tag: &str) {
        for m in our_modules() {
            println!("{tag} module: {m}");
        }
    }

    fn watchdog(mode: &'static str, secs: u64) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            println!("RESULT {mode}: FAIL timeout after {secs}s (possible deadlock)");
            std::process::exit(3);
        });
    }

    fn verdict(name: &str, ok: bool, detail: impl std::fmt::Display) -> bool {
        println!("RESULT {name}: {} {detail}", if ok { "PASS" } else { "FAIL" });
        ok
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        let mode = args.get(1).map(String::as_str).unwrap_or("");
        let arg = |i: usize| PathBuf::from(args.get(i).unwrap_or_else(|| panic!("missing arg {i}")));
        println!("host exe: {}", std::env::current_exe().unwrap().display());
        println!("host cwd: {}", std::env::current_dir().unwrap().display());
        let code = match mode {
            "w8" => w8(&arg(2)),
            "w2" => w2(&arg(2), &arg(3)),
            "w3" => w3(&arg(2)),
            "w5" => w5(&arg(2)),
            "w5bad" => w5bad(&arg(2)),
            "w7" => w7(&arg(2), &arg(3)),
            _ => panic!("unknown mode {mode}"),
        };
        std::process::exit(code);
    }

    fn w8(plugin: &Path) -> i32 {
        watchdog("W8", 90);
        let h = load(plugin).unwrap_or_else(|e| panic!("load {}: error {e}", plugin.display()));
        print_modules("W8");
        let mut all = true;

        // TypeId of a shared type.
        let type_id: fn() -> TypeId = unsafe { sym(h, "greeter_type_id") };
        all &= verdict("W8.typeid", type_id() == TypeId::of::<Shared>(), format!(
            "host={:?} plugin={:?}",
            TypeId::of::<Shared>(),
            type_id()
        ));

        // Downcast in both directions.
        let make_any: fn() -> Box<dyn Any + Send> = unsafe { sym(h, "greeter_any") };
        let from_plugin = make_any().downcast::<Shared>().ok().map(|s| s.0);
        let downcast: fn(Box<dyn Any + Send>) -> Option<u32> = unsafe { sym(h, "greeter_downcast") };
        let in_plugin = downcast(Box::new(Shared(5)));
        all &= verdict(
            "W8.downcast",
            from_plugin == Some(42) && in_plugin == Some(5),
            format!("plugin->host={from_plugin:?} host->plugin={in_plugin:?}"),
        );

        // tokio::spawn inside the plugin uses the host runtime.
        let spawn: fn(tokio::sync::mpsc::UnboundedSender<String>) -> bool =
            unsafe { sym(h, "greeter_spawn") };
        let (tx0, _rx0) = tokio::sync::mpsc::unbounded_channel();
        let outside = spawn(tx0);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("host-worker")
            .enable_all()
            .build()
            .unwrap();
        let msgs = rt.block_on(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let in_block_on = spawn(tx.clone());
            // Also call it from inside a worker task.
            let in_task = tokio::spawn(async move { spawn(tx) }).await.unwrap();
            let mut got = Vec::new();
            while got.len() < 4 {
                match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                    Ok(Some(m)) => got.push(m),
                    _ => break,
                }
            }
            (in_block_on, in_task, got)
        });
        let (in_block_on, in_task, got) = msgs;
        let spawn_ok = !outside
            && in_block_on
            && in_task
            && got.len() == 4
            && got.iter().all(|m| m.ends_with("host-worker"));
        all &= verdict(
            "W8.tokio_spawn",
            spawn_ok,
            format!("no_runtime_outside={} try_current_in_block_on={in_block_on} in_task={in_task} tasks={got:?}", !outside),
        );
        drop(rt);

        // thread_local! defined in the SDK is one instance.
        let tl_read: fn() -> (u64, u64) = unsafe { sym(h, "greeter_tl_read") };
        let tl_write: fn(u64) = unsafe { sym(h, "greeter_tl_write") };
        let tl_addr: fn() -> (usize, usize) = unsafe { sym(h, "greeter_tl_addr") };
        let check_tls = move |label: &str| -> bool {
            sdk::TL.with(|c| c.set(7));
            sdk::TL_LAZY.with(|c| c.set(8));
            let seen = tl_read();
            tl_write(11);
            let back = (sdk::TL.with(|c| c.get()), sdk::TL_LAZY.with(|c| c.get()));
            let host_addr = sdk::TL.with(|c| c as *const _ as usize);
            let (plugin_inline, plugin_via_sdk) = tl_addr();
            let ok = seen == (7, 8)
                && back == (11, 12)
                && host_addr == plugin_inline
                && host_addr == plugin_via_sdk
                && host_addr == sdk::tl_addr();
            println!(
                "W8 tls[{label}]: plugin saw {seen:?}, host saw {back:?}, addr host={host_addr:#x} plugin={plugin_inline:#x} plugin_via_sdk={plugin_via_sdk:#x}"
            );
            ok
        };
        let main_ok = check_tls("main");
        let other_ok = std::thread::spawn(move || check_tls("thread2")).join().unwrap();
        all &= verdict("W8.thread_local", main_ok && other_ok, format!("main={main_ok} other_thread={other_ok}"));

        // Shared static.
        let bump: fn() -> usize = unsafe { sym(h, "greeter_counter_bump") };
        sdk::COUNTER.store(100, Ordering::SeqCst);
        let after = bump();
        all &= verdict("W8.shared_static", after == 101 && sdk::COUNTER.load(Ordering::SeqCst) == 101, format!("after_plugin_bump={after}"));

        // Panic in the plugin caught by the host.
        let panic_fn: fn() = unsafe { sym(h, "greeter_panic") };
        let caught = std::panic::catch_unwind(panic_fn);
        let message = match &caught {
            Err(payload) => payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string payload>".into()),
            Ok(()) => "<no panic>".into(),
        };
        let version: fn() -> &'static str = unsafe { sym(h, "greeter_version") };
        let still_works = version() == "1" || version() == "2";
        all &= verdict(
            "W8.catch_unwind",
            caught.is_err() && message.contains("boom") && still_works,
            format!("payload={message:?} plugin_usable_after={still_works}"),
        );

        // Drop of a plugin type runs exactly once when the host drops it.
        let make: fn() -> Box<dyn Greeter> = unsafe { sym(h, "greeter_make") };
        sdk::DROPS.store(0, Ordering::SeqCst);
        let g = make();
        let text = g.greet();
        let before = sdk::DROPS.load(Ordering::SeqCst);
        drop(g);
        let drops = sdk::DROPS.load(Ordering::SeqCst);
        all &= verdict("W8.drop", before == 0 && drops == 1, format!("greet={text:?} drops_before={before} drops_after={drops}"));

        println!("RESULT W8: {}", if all { "PASS (all checks)" } else { "FAIL (see above)" });
        if all { 0 } else { 1 }
    }

    fn w2(a: &Path, b: &Path) -> i32 {
        watchdog("W2", 60);
        let ha = load(a).unwrap_or_else(|e| panic!("load a: {e}"));
        let hb = load(b).unwrap_or_else(|e| panic!("load b: {e}"));
        let ha2 = load(a).unwrap_or_else(|e| panic!("reload a: {e}"));
        print_modules("W2");
        let va: fn() -> &'static str = unsafe { sym(ha, "greeter_version") };
        let vb: fn() -> &'static str = unsafe { sym(hb, "greeter_version") };
        let ma: fn() -> Box<dyn Greeter> = unsafe { sym(ha, "greeter_make") };
        let mb: fn() -> Box<dyn Greeter> = unsafe { sym(hb, "greeter_make") };
        let (ga, gb) = (ma(), mb());
        println!("W2 a handle={ha:?} path={} version={} greet={:?}", module_path(ha), va(), ga.greet());
        println!("W2 b handle={hb:?} path={} version={} greet={:?}", module_path(hb), vb(), gb.greet());
        println!("W2 a reloaded handle={ha2:?} (same as first: {})", ha2 == ha);
        let ok = ha != hb && va() == "1" && vb() == "2" && ga.version() == "1" && gb.version() == "2" && ha2 == ha;
        verdict("W2", ok, format!("separate_modules={} versions=({},{})", ha != hb, va(), vb()));
        if ok { 0 } else { 1 }
    }

    fn w3(plugin: &Path) -> i32 {
        watchdog("W3", 60);
        println!("W3 host static-import sdk mark: {}", sdk::mark());
        print_modules("W3 before plugin");
        let h = match load(plugin) {
            Ok(h) => h,
            Err(e) => {
                println!("RESULT W3.case: plugin load failed error={e} host_mark={}", sdk::mark());
                return 1;
            }
        };
        print_modules("W3 after plugin");
        let mark: fn() -> &'static str = unsafe { sym(h, "greeter_sdk_mark") };
        let bump: fn() -> usize = unsafe { sym(h, "greeter_counter_bump") };
        sdk::COUNTER.store(41, Ordering::SeqCst);
        let n = bump();
        println!(
            "RESULT W3.case: host_mark={} plugin_mark={} one_sdk={}",
            sdk::mark(),
            mark(),
            n == 42
        );
        0
    }

    fn w5(plugin: &Path) -> i32 {
        watchdog("W5", 90);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("host-worker")
            .thread_keep_alive(Duration::from_millis(5))
            .enable_all()
            .build()
            .unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Background load: workers busy with timers and TLS, blocking-pool
        // threads constantly starting and exiting (thread attach/detach take
        // the loader lock too).
        for i in 0..8u64 {
            let stop = stop.clone();
            rt.spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    sdk::TL.with(|c| c.set(c.get() + i));
                    let _ = tokio::task::spawn_blocking(move || {
                        std::thread::sleep(Duration::from_millis(1));
                        sdk::TL.with(|c| c.get())
                    })
                    .await;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            });
        }
        std::thread::sleep(Duration::from_millis(200));
        let path = plugin.to_path_buf();
        let loaded = rt.block_on(async move {
            let started = std::time::Instant::now();
            let h = tokio::time::timeout(
                Duration::from_secs(30),
                tokio::task::spawn_blocking(move || load(&path).map(|h| h as usize)),
            )
            .await;
            (h, started.elapsed())
        });
        let (h, took) = loaded;
        let h = match h {
            Ok(Ok(Ok(h))) => h as HMODULE,
            other => {
                println!("RESULT W5: FAIL load did not complete: {other:?}");
                return 1;
            }
        };
        println!("W5 load took {took:?} while runtime busy");
        let info: fn() -> (bool, String) = unsafe { sym(h, "w5_ctor_info") };
        let touch: fn() = unsafe { sym(h, "w5_touch") };
        let counts: fn() -> (usize, usize) = unsafe { sym(h, "w5_tls_counts") };
        let (ctor_ran, ctor_thread) = info();
        println!("W5 ctor ran={ctor_ran} on thread {ctor_thread}; counts after load={:?}", counts());
        // Touch the destructor-bearing TLS on workers and on short-lived threads.
        rt.block_on(async move {
            let mut tasks = Vec::new();
            for _ in 0..64 {
                tasks.push(tokio::spawn(async move {
                    touch();
                    tokio::task::yield_now().await;
                }));
                tasks.push(tokio::task::spawn_blocking(move || touch()));
            }
            for t in tasks {
                t.await.unwrap();
            }
        });
        for _ in 0..16 {
            std::thread::spawn(move || touch()).join().unwrap();
        }
        let mid = counts();
        stop.store(true, Ordering::Relaxed);
        rt.shutdown_timeout(Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(200));
        let end = counts();
        println!("W5 tls (inits, dtors): after short threads={mid:?} after runtime shutdown={end:?}");
        let ok = ctor_ran && end.1 >= 16 && end.1 > mid.1;
        verdict("W5", ok, format!("ctor_ran={ctor_ran} load_took={took:?} tls_dtors={} no_deadlock=true", end.1));
        if ok { 0 } else { 1 }
    }

    fn w5bad(plugin: &Path) -> i32 {
        watchdog("W5bad", 60);
        let started = std::time::Instant::now();
        let h = load(plugin).unwrap_or_else(|e| panic!("load: {e}"));
        let outcome: fn() -> u8 = unsafe { sym(h, "w5bad_outcome") };
        let text = match outcome() {
            1 => "thread answered inside DllMain",
            2 => "wait timed out: a thread started in DllMain cannot run until it returns",
            _ => "initializer did not run",
        };
        println!("RESULT W5bad: outcome={} ({text}) load_took={:?}", outcome(), started.elapsed());
        0
    }

    fn w7(plugin: &Path, workdir: &Path) -> i32 {
        watchdog("W7", 60);
        let a_dir = workdir.join("cache").join("aaaa");
        let b_dir = workdir.join("cache").join("bbbb");
        std::fs::create_dir_all(&a_dir).unwrap();
        std::fs::create_dir_all(&b_dir).unwrap();
        let a = a_dir.join("greeter.dll");
        std::fs::copy(plugin, &a).unwrap();
        let ha = load(&a).unwrap_or_else(|e| panic!("load a: {e}"));
        let err = |r: std::io::Result<()>| match r {
            Ok(()) => "succeeded".to_string(),
            Err(e) => format!("denied (os error {:?}: {e})", e.raw_os_error()),
        };
        let overwrite = err(std::fs::write(&a, b"garbage"));
        let copy_over = err(std::fs::copy(plugin, &a).map(|_| ()));
        let delete = err(std::fs::remove_file(&a));
        let moved = a_dir.join("greeter.moved.dll");
        let rename = err(std::fs::rename(&a, &moved));
        if moved.exists() {
            println!("W7 note: rename of the loaded DLL succeeded; module path now {}", module_path(ha));
        }
        let rmdir = err(std::fs::remove_dir_all(&a_dir));
        let b = b_dir.join("greeter.dll");
        std::fs::copy(plugin, &b).unwrap();
        let hb = load(&b);
        let new_ok = match hb {
            Ok(hb) => {
                let v: fn() -> &'static str = unsafe { sym(hb, "greeter_version") };
                println!("W7 new path load: handle={hb:?} distinct={} version={}", hb != ha, v());
                hb != ha
            }
            Err(e) => {
                println!("W7 new path load failed: {e}");
                false
            }
        };
        println!("W7 overwrite={overwrite}");
        println!("W7 copy_over={copy_over}");
        println!("W7 delete={delete}");
        println!("W7 rename={rename}");
        println!("W7 remove_dir_all={rmdir}");
        let ok = overwrite.starts_with("denied") && copy_over.starts_with("denied") && delete.starts_with("denied") && new_ok;
        verdict("W7", ok, format!("overwrite/copy/delete denied, new content-addressed path loads={new_ok}; rename: {}", if rename.starts_with("succeeded") { "allowed" } else { "denied" }));
        if ok { 0 } else { 1 }
    }
}
