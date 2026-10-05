//! WebSocket failures across implementations: Rust and Node, each dialing
//! the other, classify refusals the same way (credentials and certificates
//! are authentication failures, a subprotocol mismatch is incompatible).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rutis_bridge::{
    Credential, Dial, Identity, Registered, Registration, StaticIdentity, Transport,
};
use rutis_channel::{ConnectError, PeerId};
use rutis_transport_websocket::{Config, ListenerConfig, ServerTls, Trust, WebSocketTransport};
use tokio::io::AsyncBufReadExt;

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

fn node_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node")
}

/// A CA and a certificate for localhost signed by it, as PEM.
fn pki() -> (String, String, String) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    // Distinct names: OpenSSL takes a certificate whose issuer is its own
    // subject for self-signed.
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "rutis test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");
    let certificate = params.signed_by(&key, &issuer).unwrap();
    (ca.pem(), certificate.pem(), key.serialize_pem())
}

async fn node_dial(url: &str, protocol: &str, env: &[(&str, &str)]) -> String {
    let output = tokio::process::Command::new("node")
        .args(["--import", "tsx"])
        .arg(node_dir().join("test/fixtures/ws-probe.mjs"))
        .args(["dial", url, protocol])
        .envs(env.iter().copied())
        .current_dir(node_dir())
        .output()
        .await
        .unwrap();
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn node_listen(url: &str, env: &[(&str, &str)]) -> (tokio::process::Child, String) {
    let mut child = tokio::process::Command::new("node")
        .args(["--import", "tsx"])
        .arg(node_dir().join("test/fixtures/ws-probe.mjs"))
        .args(["listen", url])
        .envs(env.iter().copied())
        .current_dir(node_dir())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let address = loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("the listener's address");
        if let Some(address) = line.strip_prefix("rutis-interop: listening on ") {
            break address.to_owned();
        }
    };
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    (child, address)
}

fn register(transport: &WebSocketTransport, protocol: &str) -> Registered {
    transport
        .register(Registration {
            listener: "public".into(),
            peer: id("node"),
            identity: Arc::new(StaticIdentity::new(id("main")).accept_token("good", id("node"))),
            protocol: protocol.into(),
            deliver: Box::new(|_| {}),
        })
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn node_dialing_rust_is_refused_by_category() {
    let (ca, certificate, key) = pki();
    let plain = WebSocketTransport::start(Config::new().listener(ListenerConfig::new(
        "public",
        "127.0.0.1:0".parse().unwrap(),
        id("main"),
    )))
    .unwrap();
    let _r = register(&plain, "rutis.3");
    let url = format!("ws://{}/rutis", plain.local_addr("public").unwrap());
    assert_eq!(
        node_dial(&url, "rutis.3", &[("RUTIS_INTEROP_TOKEN", "good")]).await,
        "connected"
    );
    assert_eq!(
        node_dial(&url, "rutis.3", &[("RUTIS_INTEROP_TOKEN", "bad")]).await,
        "auth-rejected"
    );
    assert_eq!(node_dial(&url, "rutis.3", &[]).await, "auth-rejected");
    assert_eq!(
        node_dial(&url, "rutis.9", &[("RUTIS_INTEROP_TOKEN", "good")]).await,
        "incompatible"
    );

    // wss: verified against the CA, refused without it.
    let secure = WebSocketTransport::start(Config::new().listener(
        ListenerConfig::new("public", "127.0.0.1:0".parse().unwrap(), id("main")).tls(ServerTls {
            certificate_pem: certificate.into_bytes(),
            key_pem: key.into_bytes(),
            client_ca_pem: None,
        }),
    ))
    .unwrap();
    let _s = register(&secure, "rutis.3");
    let url = format!(
        "wss://localhost:{}/rutis",
        secure.local_addr("public").unwrap().port()
    );
    let dir = tempfile::tempdir().unwrap();
    let ca_file = dir.path().join("ca.pem");
    std::fs::write(&ca_file, ca).unwrap();
    let ca_path = ca_file.to_str().unwrap();
    assert_eq!(
        node_dial(
            &url,
            "rutis.3",
            &[
                ("RUTIS_INTEROP_TOKEN", "good"),
                ("RUTIS_INTEROP_CA", ca_path)
            ]
        )
        .await,
        "connected"
    );
    assert_eq!(
        node_dial(&url, "rutis.3", &[("RUTIS_INTEROP_TOKEN", "good")]).await,
        "auth-rejected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_dialing_node_is_refused_by_category() {
    let (ca, certificate, key) = pki();
    let client = WebSocketTransport::start(Config::new()).unwrap();
    let dial = |url: &str, token: &str, protocol: &str| {
        let identity: Arc<dyn Identity> = Arc::new(
            StaticIdentity::new(id("main")).present(id("node"), Credential::Bearer(token.into())),
        );
        Dial::address(url)
            .peer(id("node"))
            .identity(identity)
            .protocol(protocol)
    };
    let (_plain, url) =
        node_listen("ws://127.0.0.1:0/rutis", &[("RUTIS_INTEROP_TOKEN", "good")]).await;
    assert!(client.dial(&dial(&url, "good", "rutis.3")).await.is_ok());
    assert!(matches!(
        client.dial(&dial(&url, "bad", "rutis.3")).await,
        Err(ConnectError::AuthRejected { .. })
    ));
    assert!(matches!(
        client.dial(&dial(&url, "good", "rutis.9")).await,
        Err(ConnectError::Incompatible { .. })
    ));

    let dir = tempfile::tempdir().unwrap();
    let (cert_file, key_file) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_file, certificate).unwrap();
    std::fs::write(&key_file, key).unwrap();
    let (_secure, url) = node_listen(
        "wss://localhost:0/rutis",
        &[
            ("RUTIS_INTEROP_TOKEN", "good"),
            ("RUTIS_INTEROP_CERT", cert_file.to_str().unwrap()),
            ("RUTIS_INTEROP_KEY", key_file.to_str().unwrap()),
        ],
    )
    .await;
    let trusting =
        WebSocketTransport::start(Config::new().trust(Trust::only(ca.into_bytes()))).unwrap();
    assert!(trusting.dial(&dial(&url, "good", "rutis.3")).await.is_ok());
    assert!(matches!(
        client.dial(&dial(&url, "good", "rutis.3")).await,
        Err(ConnectError::AuthRejected { .. })
    ));
}
