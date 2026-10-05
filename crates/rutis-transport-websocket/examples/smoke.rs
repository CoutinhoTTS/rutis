//! A two-machine smoke test of the network stack, run by hand: one side
//! listens (wss), exports `clock`; the other dials, imports it and calls it
//! every second. Both print every change of their link, so cutting the
//! network, restarting either side or presenting a wrong token shows what a
//! link does about it.
//!
//! ```text
//! # on the listening machine (certificate and key for its host name)
//! cargo run -p rutis-transport-websocket --example smoke -- \
//!     listen 0.0.0.0:7443 --cert server.pem --key server.key --token secret
//!
//! # on the dialing machine (the CA that signed the server's certificate)
//! cargo run -p rutis-transport-websocket --example smoke -- \
//!     dial wss://server.example:7443/rutis --ca ca.pem --token secret
//! ```
//!
//! On one machine, `listen 127.0.0.1:7443 --token secret` and
//! `dial ws://127.0.0.1:7443/rutis --token secret` need no certificates.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rutis::Ctx;
use rutis_bridge::{
    Credential, ExportPlugin, IdentityPlugin, ImportPlugin, LinkConfig, LinkPlugin, LinkState,
    StaticIdentity,
};
use rutis_channel::PeerId;
use rutis_interop::rpc::{Reply, Value};
use rutis_interop::{host_key, HostDispatch};
use rutis_transport_websocket::{Config, ListenerConfig, ServerTls, Trust, WebSocketPlugin};
use serde_json::{json, Value as Json};

struct Clock(AtomicU64);
impl HostDispatch for Clock {
    fn invoke(&self, _: &str, _: Value) -> Reply {
        Ok(json!(self.0.fetch_add(1, Ordering::SeqCst)).into())
    }
    fn methods(&self) -> Option<Json> {
        Some(json!({ "now": "sync" }))
    }
}

struct Args {
    mode: String,
    address: String,
    cert: Option<String>,
    key: Option<String>,
    ca: Option<String>,
    token: String,
}

fn args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: smoke listen <bind> [--cert pem --key pem] --token t | dial <url> [--ca pem] --token t";
    let mode = args.next().ok_or(usage)?;
    let address = args.next().ok_or(usage)?;
    let (mut cert, mut key, mut ca, mut token) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(usage)?;
        match flag.as_str() {
            "--cert" => cert = Some(value),
            "--key" => key = Some(value),
            "--ca" => ca = Some(value),
            "--token" => token = Some(value),
            _ => return Err(usage.into()),
        }
    }
    Ok(Args {
        mode,
        address,
        cert,
        key,
        ca,
        token: token.ok_or(usage)?,
    })
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// Print every state the link goes through, with the time since start.
fn report(link: &LinkPlugin, started: Instant) {
    let mut state = link.state();
    tokio::spawn(async move {
        loop {
            let now = state.borrow_and_update().clone();
            let line = match now {
                LinkState::Ready { generation } => format!("ready, session {generation}"),
                LinkState::Waiting {
                    failure,
                    error,
                    retry_in,
                } => format!("waiting {retry_in:.1?} after {failure:?}: {error}"),
                other => format!("{other:?}"),
            };
            println!("[{:>7.1?}] link: {line}", started.elapsed());
            if state.changed().await.is_err() {
                return;
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = args()?;
    let started = Instant::now();
    let (main_id, mac_id) = (PeerId::new("main")?, PeerId::new("mac")?);
    let root = Ctx::root()?;

    match args.mode.as_str() {
        "listen" => {
            let mut listener =
                ListenerConfig::new("public", args.address.parse()?, main_id.clone());
            if let (Some(cert), Some(key)) = (&args.cert, &args.key) {
                listener = listener.tls(ServerTls {
                    certificate_pem: read(cert),
                    key_pem: read(key),
                    client_ca_pem: None,
                });
            }
            (&root.plugin(WebSocketPlugin::new(Config::new().listener(listener))?)).await?;
            root.provide_as::<dyn HostDispatch>(
                host_key("clock"),
                Arc::new(Clock(AtomicU64::new(0))),
            )?;
            root.plugin(IdentityPlugin::new(
                "main",
                StaticIdentity::new(main_id).accept_token(args.token.as_str(), mac_id.clone()),
            ));
            let link = LinkPlugin::new(LinkConfig::listen(
                mac_id.clone(),
                "websocket",
                "main",
                "public",
            ));
            report(&link, started);
            root.plugin(link);
            root.plugin(ExportPlugin::new(mac_id, ["clock"]));
            println!("listening on {}; exporting clock", args.address);
            tokio::signal::ctrl_c().await?;
        }
        "dial" => {
            let mut config = Config::new();
            if let Some(ca) = &args.ca {
                config = config.trust(Trust::only(read(ca)));
            }
            (&root.plugin(WebSocketPlugin::new(config)?)).await?;
            root.plugin(IdentityPlugin::new(
                "mac",
                StaticIdentity::new(mac_id)
                    .present(main_id.clone(), Credential::Bearer(args.token.clone())),
            ));
            let link = LinkPlugin::new(LinkConfig::dial(
                main_id.clone(),
                "websocket",
                "mac",
                &args.address,
            ));
            report(&link, started);
            root.plugin(link);
            root.plugin(ImportPlugin::new(main_id, ["clock"]));
            let mut ticks = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = ticks.tick() => {}
                    _ = tokio::signal::ctrl_c() => break,
                }
                let Some(clock) = root.get_as::<dyn HostDispatch>(host_key("clock")) else {
                    println!("[{:>7.1?}] clock: not imported", started.elapsed());
                    continue;
                };
                let sent = Instant::now();
                let now =
                    tokio::task::spawn_blocking(move || clock.invoke("now", Value::List(vec![])))
                        .await?;
                match now.and_then(Value::json) {
                    Ok(now) => println!(
                        "[{:>7.1?}] clock: {now} in {:.1?}",
                        started.elapsed(),
                        sent.elapsed()
                    ),
                    Err(error) => println!("[{:>7.1?}] clock: {error}", started.elapsed()),
                }
            }
        }
        _ => return Err("the mode is listen or dial".into()),
    }
    root.shutdown().await?;
    Ok(())
}
