//! A native contract difference, not a cross-process routing implementation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rutis::{Ctx, Event, EventKey, SyncEvent};

struct Decision;

impl Event for Decision {
    const NAME: &'static str = "decision";
    type Value = bool;
}

impl SyncEvent for Decision {}

#[tokio::test(flavor = "current_thread")]
async fn rutis_some_false_stops_while_cordis_false_continues() {
    let ctx = Ctx::root().unwrap();
    let key = EventKey::<Decision>::of();
    let later = Arc::new(AtomicBool::new(false));
    ctx.events()
        .on_sync(&ctx, &key, |_: &Ctx, _: &Decision| Ok(Some(false)))
        .unwrap();
    let observed = later.clone();
    ctx.events()
        .on_sync(&ctx, &key, move |_: &Ctx, _: &Decision| {
            observed.store(true, Ordering::SeqCst);
            Ok(Some(true))
        })
        .unwrap();
    assert_eq!(
        ctx.events().bail_sync(&ctx, &key, &Decision).unwrap(),
        Some(false)
    );
    assert!(!later.load(Ordering::SeqCst));
    ctx.shutdown().await.unwrap();

    let output = tokio::process::Command::new("node")
        .args([
            "--input-type=module",
            "-e",
            r#"
            import { Context } from '@deepseek-ai/cordis';
            const ctx = new Context();
            let later = false;
            ctx.on('decision', () => false);
            ctx.on('decision', () => { later = true; return true; });
            const value = ctx.bail('decision');
            await ctx.fiber.dispose();
            console.log(JSON.stringify({ value, later }));
        "#,
        ])
        .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node"))
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({ "value": true, "later": true })
    );
}
