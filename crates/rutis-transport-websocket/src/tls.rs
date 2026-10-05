//! rustls configurations for dialing and listening.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};

use crate::{ServerTls, Trust};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    let certificates = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("invalid certificate PEM: {error}"))?;
    if certificates.is_empty() {
        return Err("no certificate in PEM".into());
    }
    Ok(certificates)
}

fn key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, String> {
    PrivateKeyDer::from_pem_slice(pem).map_err(|error| format!("invalid private key PEM: {error}"))
}

/// The roots a dialing side trusts.
pub(crate) fn roots(trust: &Trust) -> Result<Arc<RootCertStore>, String> {
    let mut roots = RootCertStore::empty();
    if trust.system {
        // Certificates the platform cannot parse are skipped, as browsers do.
        for certificate in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(certificate);
        }
    }
    for pem in &trust.ca_pem {
        for certificate in certificates(pem)? {
            roots
                .add(certificate)
                .map_err(|error| format!("invalid CA certificate: {error}"))?;
        }
    }
    Ok(Arc::new(roots))
}

/// A client configuration, presenting a client certificate when given one.
pub(crate) fn client(
    roots: Arc<RootCertStore>,
    certificate: Option<(&[u8], &[u8])>,
) -> Result<Arc<ClientConfig>, String> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_root_certificates(roots);
    let config = match certificate {
        Some((chain, key_pem)) => builder
            .with_client_auth_cert(certificates(chain)?, key(key_pem)?)
            .map_err(|error| format!("invalid client certificate: {error}"))?,
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// A listener's server configuration. With a client CA, client
/// certificates are verified against it but not required: a far end may
/// authenticate with a bearer token instead.
pub(crate) fn server(tls: &ServerTls) -> Result<Arc<ServerConfig>, String> {
    let builder = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?;
    let builder = match &tls.client_ca_pem {
        Some(pem) => {
            let mut roots = RootCertStore::empty();
            for certificate in certificates(pem)? {
                roots
                    .add(certificate)
                    .map_err(|error| format!("invalid client CA certificate: {error}"))?;
            }
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
                .allow_unauthenticated()
                .build()
                .map_err(|error| error.to_string())?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let config = builder
        .with_single_cert(certificates(&tls.certificate_pem)?, key(&tls.key_pem)?)
        .map_err(|error| format!("invalid server certificate: {error}"))?;
    Ok(Arc::new(config))
}
