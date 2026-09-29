//! Callbacks and returned functions of published plugins: Rust closures are
//! called back by Cordis, and functions Cordis returns are called from Rust.
#![cfg(all(unix, dsh_baseline))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dsh_baseline::{credentials, fs, invariants, jobs};
use rutis::{Ctx, Plugin};
use rutis_interop::serde_json::json;

async fn mount(ctx: &Ctx, plugin: impl Plugin + 'static) {
    let view = ctx.plugin(plugin);
    (&view).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cordis_calls_rust_closures_and_rust_calls_returned_functions() {
    let dir = tempfile::tempdir().unwrap();
    let (home, work) = (dir.path().join("home"), dir.path().join("work"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("a.txt"), "hello").unwrap();
    let ctx = Ctx::root().unwrap();
    mount(&ctx, invariants::Plugin::new(Default::default())).await;
    mount(
        &ctx,
        credentials::Plugin::new(credentials::Config {
            dsh_home: Some(home.to_str().unwrap().into()),
            watch: Some(false),
            ..Default::default()
        }),
    )
    .await;
    mount(
        &ctx,
        fs::Plugin::new(fs::Config {
            cwd: Some(work.to_str().unwrap().into()),
            ..Default::default()
        }),
    )
    .await;
    mount(&ctx, jobs::Plugin::new(Default::default())).await;

    // An asynchronous installer receives Cordis's child context; the returned
    // function unregisters it.
    let installed = Arc::new(AtomicUsize::new(0));
    let seen = installed.clone();
    let registry = ctx.get::<invariants::InvariantRegistry>().unwrap();
    let unregister = registry
        .register("@example/checks", move |_ctx, _fail| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        })
        .unwrap();
    for _ in 0..100 {
        if installed.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(installed.load(Ordering::SeqCst), 1);
    unregister.call(vec![]).unwrap();

    // An asynchronous callback's result is what Cordis stores.
    let store = ctx.get::<credentials::CredentialProvider>().unwrap();
    let key = credentials::CredentialKey::from("example/token");
    let written = store
        .modify_record(&key, |current| {
            Box::pin(async move {
                assert!(current.is_none());
                Ok(Some(json!({ "kind": "api-key", "key": "s3cret" })))
            })
        })
        .await
        .unwrap();
    assert_eq!(written, Some(json!({ "kind": "api-key", "key": "s3cret" })));
    assert_eq!(
        store.read_record(&key).await.unwrap(),
        Some(json!({ "kind": "api-key", "key": "s3cret" }))
    );

    // A stored callback is called later; the returned async function stops it.
    let files = ctx.get::<fs::FileSystem>().unwrap();
    let file = files.resolve("a.txt", None).await.unwrap();
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = changes.clone();
    let stop = files
        .watch(&file, move |_error| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .unwrap();
    for attempt in 0..100 {
        if changes.load(Ordering::SeqCst) > 0 {
            break;
        }
        std::fs::write(work.join("a.txt"), format!("change {attempt}")).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        changes.load(Ordering::SeqCst) > 0,
        "the watch callback never ran"
    );
    stop.call_async(vec![]).await.unwrap();

    // A returned synchronous function.
    let detach = ctx
        .get::<jobs::JobRegistry>()
        .unwrap()
        .attach_controller("example")
        .unwrap();
    detach.call(vec![]).unwrap();

    drop((registry, store, files, unregister, stop, detach));
    ctx.shutdown().await.unwrap();
}
