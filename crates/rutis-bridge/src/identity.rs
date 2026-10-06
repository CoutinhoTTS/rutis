//! Who an endpoint is, what it presents when it dials, and how what a far
//! end presents maps to that far end's endpoint id. Transports apply these
//! rules; an identity never sees the connection itself.

use std::collections::HashMap;
use std::sync::Arc;

use crate::channel::PeerId;
use rutis::TypeKey;

/// A credential presented when dialing. Its `Debug` never shows the secret.
#[derive(Clone)]
pub enum Credential {
    /// Sent as `Authorization: Bearer <token>`; never in a URL or a log.
    Bearer(String),
    /// A TLS client certificate chain and its private key, both PEM.
    ClientCertificate {
        chain_pem: Vec<u8>,
        key_pem: Vec<u8>,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(_) => f.write_str("Bearer(…)"),
            Self::ClientCertificate { .. } => f.write_str("ClientCertificate(…)"),
        }
    }
}

/// What a far end presented, as the transport saw it.
#[derive(Clone, Copy)]
pub enum Presented<'a> {
    Bearer(&'a str),
    /// The DER of its verified leaf certificate.
    Certificate(&'a [u8]),
}

pub trait Identity: Send + Sync + 'static {
    /// This endpoint's id.
    fn local(&self) -> &PeerId;

    /// What to present when dialing `peer`.
    fn credential(&self, peer: &PeerId) -> Option<Credential>;

    /// The endpoint `presented` proves, if any.
    fn verify(&self, presented: Presented<'_>) -> Option<PeerId>;
}

/// The key an identity named `name` is provided under (`Identity#main`).
pub fn identity_key(name: &str) -> TypeKey {
    TypeKey::keyed_dynamic::<dyn Identity>(name.to_owned())
}

/// Provides an identity as `Identity#<name>`.
pub struct IdentityPlugin {
    label: String,
    /// The `<name>` of `Identity#<name>`.
    key: String,
    identity: Arc<dyn Identity>,
}

impl IdentityPlugin {
    pub fn new(name: &str, identity: impl Identity) -> Self {
        Self {
            label: format!("rutis-bridge/identity#{name}"),
            key: name.to_owned(),
            identity: Arc::new(identity),
        }
    }
}

impl rutis::Plugin for IdentityPlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn apply<'a>(
        &'a self,
        ctx: &'a rutis::Ctx,
    ) -> rutis::BoxFuture<'a, Result<rutis::Effect, rutis::CordisError>> {
        Box::pin(async move {
            ctx.provide_as::<dyn Identity>(identity_key(&self.key), self.identity.clone())?;
            Ok(rutis::Effect::Done)
        })
    }
}

/// An identity from configuration: tokens and certificates per peer.
#[derive(Clone)]
pub struct StaticIdentity {
    local: PeerId,
    /// What this endpoint presents to each peer.
    outgoing: HashMap<PeerId, Credential>,
    /// Tokens peers present, mapped to who they prove.
    tokens: Vec<(Arc<str>, PeerId)>,
    /// SHA-256 fingerprints of client certificates, mapped likewise.
    certificates: Vec<([u8; 32], PeerId)>,
}

impl StaticIdentity {
    pub fn new(local: PeerId) -> Self {
        Self {
            local,
            outgoing: HashMap::new(),
            tokens: Vec::new(),
            certificates: Vec::new(),
        }
    }

    /// Present `credential` when dialing `peer`.
    pub fn present(mut self, peer: PeerId, credential: Credential) -> Self {
        self.outgoing.insert(peer, credential);
        self
    }

    /// Accept `token` as proof of `peer`.
    pub fn accept_token(mut self, token: impl Into<Arc<str>>, peer: PeerId) -> Self {
        self.tokens.push((token.into(), peer));
        self
    }

    /// Accept the client certificate with this SHA-256 fingerprint (of its
    /// DER) as proof of `peer`.
    pub fn accept_certificate(mut self, fingerprint: [u8; 32], peer: PeerId) -> Self {
        self.certificates.push((fingerprint, peer));
        self
    }
}

impl Identity for StaticIdentity {
    fn local(&self) -> &PeerId {
        &self.local
    }

    fn credential(&self, peer: &PeerId) -> Option<Credential> {
        self.outgoing.get(peer).cloned()
    }

    fn verify(&self, presented: Presented<'_>) -> Option<PeerId> {
        match presented {
            Presented::Bearer(token) => {
                // Every entry is compared, in constant time per entry, so
                // timing tells nothing about which token came close.
                let mut found = None;
                for (accepted, peer) in &self.tokens {
                    if constant_time_eq(accepted.as_bytes(), token.as_bytes()) {
                        found = Some(peer.clone());
                    }
                }
                found
            }
            Presented::Certificate(der) => {
                let fingerprint = fingerprint(der);
                self.certificates
                    .iter()
                    .find(|(accepted, _)| constant_time_eq(accepted, &fingerprint))
                    .map(|(_, peer)| peer.clone())
            }
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The SHA-256 fingerprint of a certificate's DER.
pub fn fingerprint(der: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(der).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> PeerId {
        PeerId::new(s).unwrap()
    }

    #[test]
    fn tokens_and_certificates_map_to_peers_and_secrets_stay_hidden() {
        let identity = StaticIdentity::new(id("main"))
            .present(id("mac"), Credential::Bearer("to-mac".into()))
            .accept_token("from-mac", id("mac"))
            .accept_certificate(fingerprint(b"cert of pi"), id("pi"));
        assert_eq!(identity.local(), &id("main"));
        assert_eq!(
            identity.verify(Presented::Bearer("from-mac")),
            Some(id("mac"))
        );
        assert_eq!(identity.verify(Presented::Bearer("from-ma")), None);
        assert_eq!(identity.verify(Presented::Bearer("to-mac")), None);
        assert_eq!(
            identity.verify(Presented::Certificate(b"cert of pi")),
            Some(id("pi"))
        );
        let credential = identity.credential(&id("mac")).unwrap();
        assert_eq!(format!("{credential:?}"), "Bearer(…)");
        assert!(identity.credential(&id("pi")).is_none());
    }
}
