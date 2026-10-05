//! Soak: two nodes over a loopback WebSocket, the session cut and relinked
//! over and over, a service imported and called through each session. What
//! the process holds (file descriptors, threads) must not grow with the
//! number of sessions. Ignored by default; run it with
//! `RUTIS_SOAK_SECS=600 cargo test -p rutis-transport-websocket --test soak -- --ignored`
//! (60 s when unset). Linux only: it reads `/proc`.
#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rutis::Ctx;
use rutis_bridge::{
    peer_key, Credential, ExportPlugin, IdentityPlugin, ImportPlugin, LinkConfig, LinkPlugin, Peer,
    Retry, StaticIdentity,
};
use rutis_channel::PeerId;
use rutis_interop::rpc::{Reply, Value};
use rutis_interop::{host_key, Error, HostDispatch};
use rutis_transport_websocket::{Config, ListenerConfig, WebSocketPlugin};
use serde_json::{json, Value as Json};

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

async fn eventually<T>(mut check: impl FnMut() -> Option<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(found) = check() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

struct Clock(AtomicU64);
impl HostDispatch for Clock {
    fn invoke(&self, _: &str, _: Value) -> Reply {
        Ok(json!(self.0.fetch_add(1, Ordering::SeqCst)).into())
    }
    fn methods(&self) -> Option<Json> {
        Some(json!({ "now": "sync" }))
    }
}

/// Resident memory in KiB: reported, not asserted (allocators keep what
/// they freed).
fn resident() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// Open file descriptors and threads of this process.
fn held() -> (usize, usize) {
    let fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
    let threads = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (fds, threads)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "soak: run with --ignored, RUTIS_SOAK_SECS sets the duration"]
async fn relinking_over_and_over_holds_nothing_more() {
    let duration = Duration::from_secs(
        std::env::var("RUTIS_SOAK_SECS")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .unwrap_or(60),
    );
    let quick = Retry {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(50),
        ..Retry::default()
    };

    let main = Ctx::root().unwrap();
    let websocket = WebSocketPlugin::new(Config::new().listener(ListenerConfig::new(
        "public",
        "127.0.0.1:0".parse().unwrap(),
        id("main"),
    )))
    .unwrap();
    let handle = websocket.clone();
    (&main.plugin(websocket)).await.unwrap();
    let address = format!(
        "ws://{}/rutis",
        handle.transport().unwrap().local_addr("public").unwrap()
    );
    main.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock(AtomicU64::new(0))))
        .unwrap();
    main.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main")).accept_token("mac-token", id("mac")),
    ));
    main.plugin(LinkPlugin::new(
        LinkConfig::listen(id("mac"), "websocket", "main", "public").retry(quick.clone()),
    ));
    main.plugin(ExportPlugin::new(id("mac"), ["clock"]));

    let mac = Ctx::root().unwrap();
    (&mac.plugin(WebSocketPlugin::new(Config::new()).unwrap()))
        .await
        .unwrap();
    mac.plugin(IdentityPlugin::new(
        "mac",
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
    ));
    mac.plugin(LinkPlugin::new(
        LinkConfig::dial(id("main"), "websocket", "mac", &address).retry(quick),
    ));
    mac.plugin(ImportPlugin::new(id("main"), ["clock"]));

    let started = Instant::now();
    let mut baseline = None;
    let mut sessions = 0u64;
    let mut generation = 0;
    while started.elapsed() < duration {
        // A new session, the clock imported through it and called.
        let peer = eventually(
            || {
                mac.get_as::<Peer>(peer_key(&id("main")))
                    .filter(|peer| peer.generation() > generation)
            },
            "a new session",
        )
        .await;
        generation = peer.generation();
        let clock = eventually(
            || mac.get_as::<dyn HostDispatch>(host_key("clock")),
            "the clock",
        )
        .await;
        tokio::task::spawn_blocking(move || clock.invoke("now", Value::List(vec![])))
            .await
            .unwrap()
            .expect("the clock answers");
        sessions += 1;
        // Measured once the runtimes' pools have grown to what they use.
        if sessions == 20 {
            baseline = Some((held(), resident()));
        }
        peer.connection().close(Error::Transport("soak".into()));
    }

    let (fds, threads) = held();
    let ((base_fds, base_threads), base_rss) = baseline.expect("enough sessions to measure");
    eprintln!(
        "soak: {sessions} sessions in {:?}; fds {base_fds} -> {fds}, threads {base_threads} -> {threads}, resident {base_rss} -> {} KiB",
        started.elapsed(),
        resident()
    );
    assert!(
        fds <= base_fds + 16,
        "file descriptors grew: {base_fds} -> {fds}"
    );
    assert!(
        threads <= base_threads + 16,
        "threads grew: {base_threads} -> {threads}"
    );
}
