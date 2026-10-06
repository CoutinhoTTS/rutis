//! Events of a published plugin forwarded to rutis listeners.
#![cfg(all(unix, dsh_baseline))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use dsh_baseline::credentials::{
    self, CredentialKey, CredentialRef, CredentialsRecordUpdated, CredentialsReferenceUpdated,
};
use rutis::{BoxFuture, CordisError, Ctx, EventKey, Listener};
use rutis_bridge::cordis::serde_json::json;

#[derive(Default, Clone)]
struct Seen(Arc<Mutex<Vec<String>>>);

impl Listener<CredentialsReferenceUpdated> for Seen {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a CredentialsReferenceUpdated,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(format!("reference {}", event.r#ref));
            Ok(None)
        })
    }
}

impl Listener<CredentialsRecordUpdated> for Seen {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a CredentialsRecordUpdated,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(format!("record {}", event.key));
            Ok(None)
        })
    }
}

async fn eventually(seen: &Seen, expected: &str) {
    for _ in 0..100 {
        if seen.0.lock().unwrap().iter().any(|entry| entry == expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never saw {expected}: {:?}", seen.0.lock().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_changes_reach_rutis_listeners() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::root().unwrap();
    let seen = Seen::default();
    ctx.events()
        .on(
            &ctx,
            &EventKey::<CredentialsReferenceUpdated>::of(),
            seen.clone(),
        )
        .unwrap();
    ctx.events()
        .on(
            &ctx,
            &EventKey::<CredentialsRecordUpdated>::of(),
            seen.clone(),
        )
        .unwrap();
    let view = ctx.plugin(credentials::Plugin::new(credentials::Config {
        dsh_home: Some(dir.path().to_str().unwrap().into()),
        watch: Some(false),
        ..Default::default()
    }));
    (&view).await.unwrap();
    let store = ctx.get::<credentials::CredentialProvider>().unwrap();

    store
        .set(&CredentialRef::from("test/api-key"), "s3cret")
        .await
        .unwrap();
    eventually(&seen, "reference test/api-key").await;

    store
        .modify_record(&CredentialKey::from("example/token"), |_| {
            Box::pin(async { Ok(Some(json!({ "kind": "api-key", "key": "k" }))) })
        })
        .await
        .unwrap();
    eventually(&seen, "record example/token").await;

    drop(store);
    view.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}
