//! One dial: TCP, TLS for `wss`, the upgrade with credentials and the
//! session protocol. Every failure is classified from its type or status,
//! never from its text.

use std::net::IpAddr;
use std::sync::Arc;

use crate::channel::{Channel, ChannelInfo, ConnectError};
use crate::{Credential, Dial};
use rustls::pki_types::ServerName;
use rustls::RootCertStore;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::http::{HeaderValue, Uri};
use tokio_tungstenite::tungstenite::Error as WsError;

use crate::transport::websocket::connection::{self, Io};
use crate::transport::websocket::Limits;

fn retryable(reason: impl std::fmt::Display) -> ConnectError {
    ConnectError::Retryable {
        reason: reason.to_string(),
    }
}
fn rejected(reason: impl std::fmt::Display) -> ConnectError {
    ConnectError::AuthRejected {
        reason: reason.to_string(),
    }
}
fn incompatible(reason: impl std::fmt::Display) -> ConnectError {
    ConnectError::Incompatible {
        reason: reason.to_string(),
    }
}

fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether a TLS failure is about certificates (authentication) rather
/// than the connection.
fn certificate_problem(error: &std::io::Error) -> bool {
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        Some(rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented) => true,
        Some(rustls::Error::AlertReceived(alert)) => matches!(
            alert,
            rustls::AlertDescription::BadCertificate
                | rustls::AlertDescription::UnsupportedCertificate
                | rustls::AlertDescription::CertificateRevoked
                | rustls::AlertDescription::CertificateExpired
                | rustls::AlertDescription::CertificateUnknown
                | rustls::AlertDescription::UnknownCA
                | rustls::AlertDescription::CertificateRequired
                | rustls::AlertDescription::AccessDenied
        ),
        _ => false,
    }
}

pub(crate) async fn dial(
    dial: &Dial,
    roots: Arc<RootCertStore>,
    limits: &Limits,
    runtime: &tokio::runtime::Handle,
    live: connection::Live,
) -> Result<Channel, ConnectError> {
    let uri: Uri = dial
        .address
        .parse()
        .map_err(|error| incompatible(format!("invalid address {}: {error}", dial.address)))?;
    let secure = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        _ => {
            return Err(incompatible(format!(
                "{} is not a ws:// or wss:// address",
                dial.address
            )))
        }
    };
    let host = uri
        .host()
        .ok_or_else(|| incompatible(format!("{} names no host", dial.address)))?
        .to_owned();
    if !secure && !is_loopback(&host) {
        return Err(incompatible(format!(
            "{}: a non-loopback address needs wss://",
            dial.address
        )));
    }
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let credential = match (&dial.identity, &dial.peer) {
        (Some(identity), Some(peer)) => identity.credential(peer),
        _ => None,
    };

    let connect = async {
        let tcp = TcpStream::connect((host.trim_start_matches('[').trim_end_matches(']'), port))
            .await
            .map_err(|error| retryable(format!("{host}:{port}: {error}")))?;
        let _ = tcp.set_nodelay(true);
        keepalive(&tcp);
        let io: Box<dyn Io> = if secure {
            let certificate = match &credential {
                Some(Credential::ClientCertificate { chain_pem, key_pem }) => {
                    Some((chain_pem.as_slice(), key_pem.as_slice()))
                }
                _ => None,
            };
            let config = crate::transport::websocket::tls::client(roots, certificate)
                .map_err(incompatible)?;
            let name = ServerName::try_from(
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned(),
            )
            .map_err(|error| incompatible(format!("{host}: {error}")))?;
            let tls = tokio_rustls::TlsConnector::from(config)
                .connect(name, tcp)
                .await
                .map_err(|error| match certificate_problem(&error) {
                    true => rejected(format!("{host}: TLS: {error}")),
                    false => retryable(format!("{host}: TLS: {error}")),
                })?;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };
        let mut request = dial
            .address
            .as_str()
            .into_client_request()
            .map_err(|error| incompatible(format!("{}: {error}", dial.address)))?;
        if !dial.protocol.is_empty() {
            request.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_str(&dial.protocol)
                    .map_err(|_| incompatible(format!("invalid protocol {:?}", dial.protocol)))?,
            );
        }
        if let Some(Credential::Bearer(token)) = &credential {
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| incompatible("the bearer token is not a valid header value"))?;
            value.set_sensitive(true);
            request.headers_mut().insert("Authorization", value);
        }
        let (socket, _) = tokio_tungstenite::client_async_with_config(
            request,
            io,
            Some(crate::transport::websocket::ws_config(limits)),
        )
        .await
        .map_err(|error| classify(&host, error))?;
        Ok::<_, ConnectError>(socket)
    };
    let socket = tokio::time::timeout(limits.handshake, connect)
        .await
        .map_err(|_| retryable(format!("{host}:{port}: timed out connecting")))??;
    Ok(connection::channel(
        socket,
        ChannelInfo {
            transport: "websocket",
            peer: dial.peer.clone(),
            label: host,
        },
        limits,
        runtime,
        live,
    ))
}

fn classify(host: &str, error: WsError) -> ConnectError {
    match error {
        WsError::Http(response) => {
            let status = response.status();
            let reason = format!("{host}: upgrade refused with {status}");
            match status.as_u16() {
                401 | 403 => rejected(reason),
                400 | 404 | 405 | 426 => incompatible(reason),
                _ => retryable(reason),
            }
        }
        WsError::Protocol(ProtocolError::SecWebSocketSubProtocolError(error)) => {
            incompatible(format!("{host}: session protocol: {error}"))
        }
        WsError::Io(error) if certificate_problem(&error) => rejected(format!("{host}: {error}")),
        error => retryable(format!("{host}: {error}")),
    }
}

/// TCP keepalive, beside the WebSocket heartbeat: the kernel notices dead
/// peers even while nothing is sent.
pub(crate) fn keepalive(tcp: &TcpStream) {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(std::time::Duration::from_secs(30))
        .with_interval(std::time::Duration::from_secs(10));
    let _ = socket2::SockRef::from(tcp).set_tcp_keepalive(&keepalive);
}
