//! The memory transport as a native plugin: it provides `Transport#memory`
//! while mounted, and unloading withdraws it and closes its channels.
use rutis::Ctx;
use rutis_bridge::{transport_key, Transport};
use rutis_transport_memory::MemoryPlugin;

#[tokio::test]
async fn provides_the_transport_until_unloaded() {
    let root = Ctx::root().unwrap();
    let plugin = MemoryPlugin::new();
    let listener = plugin.transport().listen("echo").unwrap();
    let fiber = root.plugin(plugin);
    (&fiber).await.unwrap();

    let transport = root
        .get_as::<dyn Transport>(transport_key("memory"))
        .expect("Transport#memory is provided");
    assert_eq!(transport.kind(), "memory");
    let mut dialed = transport.dial("echo").await.unwrap();
    let mut accepted = listener.accept().unwrap();
    dialed.sender.send(b"ping").unwrap();
    assert_eq!(accepted.receiver.recv().unwrap().unwrap(), b"ping");

    fiber.dispose().await.unwrap();
    assert!(root
        .get_as::<dyn Transport>(transport_key("memory"))
        .is_none());
    assert!(dialed.sender.send(b"after").is_err());
    assert!(matches!(accepted.receiver.recv(), Ok(None) | Err(_)));
}
