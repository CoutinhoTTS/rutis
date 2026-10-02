//! The dsh web UI started by the launcher in a rutis host.
#![cfg(all(unix, dsh_installed))]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::Scripted;
use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Listener};
use rutis_dsh::web;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

/// Hands the first event of its kind to the test.
struct First<E>(Mutex<Option<oneshot::Sender<E>>>);

impl<E: Event + Clone> Listener<E> for First<E> {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a E,
    ) -> BoxFuture<'a, Result<Option<E::Value>, CordisError>> {
        if let Some(sender) = self.0.lock().unwrap().take() {
            let _ = sender.send(event.clone());
        }
        Box::pin(async { Ok(None) })
    }
}

fn first<E: Event + Clone>(ctx: &Ctx) -> oneshot::Receiver<E> {
    let (sender, receiver) = oneshot::channel();
    ctx.events()
        .on(ctx, &EventKey::<E>::of(), First(Mutex::new(Some(sender))))
        .unwrap();
    receiver
}

/// Mounts the web profile `profile` on `port` in `ctx`.
async fn start(ctx: &Ctx, profile: &str, port: u16, workspace: &std::path::Path) {
    rutis_dsh::provide_web_aimux(ctx, Arc::new(Scripted::default())).unwrap();
    let view = ctx.plugin(web::Plugin::new(web::Config {
        profile: Some(profile.into()),
        args: Some(vec!["--no-open".into(), "--port".into(), port.to_string()]),
        cwd: Some(workspace.to_string_lossy().into_owned()),
    }));
    (&view).await.unwrap();
}

/// The status line of `GET /`, or `None` when nothing listens.
async fn status(port: u16) -> Option<String> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await.ok()?;
    response.lines().next().map(str::to_owned)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_web_ui_starts_reports_failures_and_stops_with_its_mount() {
    // dsh keeps profiles and settings under DSH_HOME; the Node process
    // inherits it. This test binary holds only this test.
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::env::set_var("DSH_HOME", home.path());
    let port = free_port();

    // Starts: the profile is up with the aimux route, and the UI answers
    // (without the startup token it refuses the request).
    let ctx = Ctx::root().unwrap();
    let ready = first::<web::RutisDshReady>(&ctx);
    start(&ctx, "rutis-web", port, workspace.path()).await;
    let ready = tokio::time::timeout(Duration::from_secs(60), ready)
        .await
        .expect("dsh started")
        .unwrap();
    assert!(
        ready.providers.iter().any(|provider| provider == "aimux"),
        "{:?}",
        ready.providers
    );
    assert!(home.path().join("profiles/rutis-web").is_dir());
    let line = status(port).await.expect("the web UI listens");
    assert!(line.contains(" 401 "), "{line}");

    // A second profile on the same port fails to start, and says why.
    let busy = Ctx::root().unwrap();
    let failed = first::<web::RutisDshStartupFailed>(&busy);
    start(&busy, "rutis-web-busy", port, workspace.path()).await;
    let failed = tokio::time::timeout(Duration::from_secs(60), failed)
        .await
        .expect("failure reported")
        .unwrap();
    assert!(failed.message.contains("EADDRINUSE"), "{}", failed.message);
    busy.shutdown().await.unwrap();

    // Disposing the mount stops dsh.
    ctx.shutdown().await.unwrap();
    assert_eq!(status(port).await, None);
}
