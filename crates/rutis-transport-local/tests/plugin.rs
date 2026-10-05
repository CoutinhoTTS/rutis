//! The local transport as a native plugin: it dials Unix sockets while
//! mounted, classifies failures by kind, and unloading closes its channels.
#![cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

use rutis::Ctx;
use rutis_bridge::{transport_key, ConnectError, Transport};
use rutis_transport_local::LocalPlugin;

#[tokio::test]
async fn dials_unix_sockets_with_newline_framing_until_unloaded() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peer.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let root = Ctx::root().unwrap();
    let fiber = root.plugin(LocalPlugin::new());
    (&fiber).await.unwrap();
    let transport = root
        .get_as::<dyn Transport>(transport_key("local"))
        .expect("Transport#local is provided");

    let address = format!("unix:{}", path.display());
    let mut channel = transport.dial(&address).await.unwrap();
    assert_eq!(channel.info.transport, "unix");
    let (mut remote, _) = listener.accept().unwrap();
    channel.sender.send(b"{\"op\":\"hello\"}").unwrap();
    let mut line = String::new();
    BufReader::new(remote.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "{\"op\":\"hello\"}\n");
    remote.write_all(b"{}\n").unwrap();
    assert_eq!(channel.receiver.recv().unwrap().unwrap(), b"{}");

    // A bare path is a Unix socket too.
    let bare = transport.dial(path.to_str().unwrap()).await.unwrap();
    drop(bare);

    fiber.dispose().await.unwrap();
    assert!(root
        .get_as::<dyn Transport>(transport_key("local"))
        .is_none());
    assert!(matches!(channel.receiver.recv(), Ok(None) | Err(_)));
}

#[tokio::test]
async fn failures_carry_their_category() {
    let transport = rutis_transport_local::LocalTransport::default();
    let missing = tempfile::tempdir().unwrap().path().join("none.sock");
    assert!(matches!(
        transport.dial(missing.to_str().unwrap()).await,
        Err(ConnectError::Retryable { .. })
    ));
    assert!(matches!(
        transport.dial("wss://example.com/rutis").await,
        Err(ConnectError::Incompatible { .. })
    ));
    assert!(matches!(
        transport.dial("fd:3").await,
        Err(ConnectError::Incompatible { .. })
    ));
}
