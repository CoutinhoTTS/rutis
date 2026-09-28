#![cfg(unix)]

use rutis_interop::server::Dispatch;
use serde_json::json;

// The executable entry is unused here; exercise its generated mount directly.
#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/rutis.rs"));

    pub async fn exports(ctx: rutis::Ctx) -> impl rutis_interop::server::Dispatch {
        mount(ctx, serde_json::json!({"initial": 7})).await.unwrap()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn captured_object_survives_the_export_context_shutdown() {
    let ctx = rutis::Ctx::root().unwrap();
    let exports = generated::exports(ctx.clone()).await;
    ctx.shutdown().await.unwrap();
    // The original Counter owns its value independently of its Context.
    assert_eq!(
        exports
            .invoke("counter", "current", json!([]).into())
            .unwrap()
            .json()
            .unwrap(),
        json!(7.0)
    );
}
