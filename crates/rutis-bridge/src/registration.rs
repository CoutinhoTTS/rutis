//! Which far ends a shared listener accepts, and for whom. Links register;
//! transports route each verified connection to the one registration that
//! names its far end, and re-check that registration at hand-over.
//!
//! Rules (`docs/design-protocol-channel-decoupling-2026-10-03.md`, shared
//! listener registration):
//! - a registration binds a listener, the far end's endpoint id, the
//!   identity whose rules verify it, a delivery target, and a generation;
//! - on one listener a far end has at most one registration;
//! - routing uses the id the transport verified, never one the far end
//!   merely claims;
//! - hand-over re-checks the generation under the same lock revocation
//!   takes, so a handshake that began before a revocation cannot complete
//!   after it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use crate::channel::{Channel, PeerId};

use crate::{Identity, Presented};

/// Where an accepted channel goes. Runs under the registry lock: it must
/// only hand the channel on (for example into a queue), never block.
pub type Deliver = Box<dyn Fn(Channel) + Send + Sync>;

/// A link's request to accept one far end on one listener.
pub struct Registration {
    /// The listener's name in the transport's configuration.
    pub listener: String,
    /// The far end to accept.
    pub peer: PeerId,
    /// Verifies what the far end presents; its local id must be the
    /// listener's.
    pub identity: Arc<dyn Identity>,
    /// The session protocol the far end must speak (`rutis.2`).
    pub protocol: String,
    pub deliver: Deliver,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistrationError {
    #[error("{0}")]
    NoListener(String),
    /// The far end already has a registration on this listener.
    #[error("{0}")]
    Duplicate(String),
    /// The identity does not belong to the listener's endpoint.
    #[error("{0}")]
    Invalid(String),
}

/// A live registration; dropping it revokes it.
pub struct Registered {
    registry: Weak<Mutex<Table>>,
    key: (String, PeerId),
    generation: u64,
}

impl Drop for Registered {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut table = registry.lock().unwrap();
            if table
                .entries
                .get(&self.key)
                .is_some_and(|entry| entry.generation == self.generation)
            {
                table.entries.remove(&self.key);
            }
        }
    }
}

struct Entry {
    generation: u64,
    identity: Arc<dyn Identity>,
    protocol: String,
    deliver: Deliver,
}

#[derive(Default)]
struct Table {
    next: u64,
    entries: HashMap<(String, PeerId), Entry>,
}

/// Why an inbound connection was turned away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Nothing verified what it presented for this listener.
    Unauthenticated,
    /// What it presented is good, but no link listens for it here (yet):
    /// nothing is registered on the listener, or this listener's identity
    /// verifies it as a peer whose link has not registered. Worth retrying.
    NotListening,
    /// More than one registration would take it.
    Ambiguous,
}

/// A verified far end and the registration generation it was routed to;
/// hand it over with [`Registrations::hand_over`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub listener: String,
    pub peer: PeerId,
    /// The session protocol the registration expects.
    pub protocol: String,
    generation: u64,
}

/// The registrations of one transport instance, shared by its listeners.
#[derive(Clone, Default)]
pub struct Registrations {
    table: Arc<Mutex<Table>>,
}

impl Registrations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `registration` for a listener whose endpoint is `local`.
    pub fn register(
        &self,
        local: &PeerId,
        registration: Registration,
    ) -> Result<Registered, RegistrationError> {
        if registration.identity.local() != local {
            return Err(RegistrationError::Invalid(format!(
                "listener {} serves endpoint {local}, not {}",
                registration.listener,
                registration.identity.local()
            )));
        }
        let key = (registration.listener.clone(), registration.peer.clone());
        let mut table = self.table.lock().unwrap();
        if table.entries.contains_key(&key) {
            return Err(RegistrationError::Duplicate(format!(
                "{} is already registered on listener {}",
                key.1, key.0
            )));
        }
        table.next += 1;
        let generation = table.next;
        table.entries.insert(
            key.clone(),
            Entry {
                generation,
                identity: registration.identity,
                protocol: registration.protocol,
                deliver: registration.deliver,
            },
        );
        Ok(Registered {
            registry: Arc::downgrade(&self.table),
            key,
            generation,
        })
    }

    /// Route a connection on `listener` by what it presented: the far end
    /// is the id an identity of this listener verifies, and must be one a
    /// registration names.
    pub fn route(&self, listener: &str, presented: Presented<'_>) -> Result<Ticket, Refusal> {
        let table = self.table.lock().unwrap();
        let mut matches = table
            .entries
            .iter()
            .filter(|((name, _), _)| name == listener)
            .filter(|((_, peer), entry)| entry.identity.verify(presented).as_ref() == Some(peer));
        let Some(((_, peer), entry)) = matches.next() else {
            let mut here = table
                .entries
                .iter()
                .filter(|((name, _), _)| name == listener)
                .peekable();
            let nothing_here = here.peek().is_none();
            let verified = here.any(|(_, entry)| entry.identity.verify(presented).is_some());
            return Err(match nothing_here || verified {
                true => Refusal::NotListening,
                false => Refusal::Unauthenticated,
            });
        };
        if matches.next().is_some() {
            return Err(Refusal::Ambiguous);
        }
        Ok(Ticket {
            listener: listener.to_owned(),
            peer: peer.clone(),
            protocol: entry.protocol.clone(),
            generation: entry.generation,
        })
    }

    /// Hand `channel` to the registration `ticket` was routed to, if it is
    /// still that same registration; otherwise give the channel back to be
    /// closed.
    pub fn hand_over(&self, ticket: &Ticket, channel: Channel) -> Result<(), Channel> {
        let table = self.table.lock().unwrap();
        match table
            .entries
            .get(&(ticket.listener.clone(), ticket.peer.clone()))
        {
            Some(entry) if entry.generation == ticket.generation => {
                (entry.deliver)(channel);
                Ok(())
            }
            _ => Err(channel),
        }
    }

    /// Whether any registration remains on `listener`.
    pub fn listens(&self, listener: &str) -> bool {
        let table = self.table.lock().unwrap();
        table.entries.keys().any(|(name, _)| name == listener)
    }

    /// Revoke every registration (the transport unloads).
    pub fn clear(&self) {
        self.table.lock().unwrap().entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StaticIdentity;
    use std::sync::mpsc;

    fn id(s: &str) -> PeerId {
        PeerId::new(s).unwrap()
    }

    fn registration(peer: &str, token: &str) -> (Registration, mpsc::Receiver<Channel>) {
        let (sender, received) = mpsc::channel();
        let sender = Mutex::new(sender);
        let identity = StaticIdentity::new(id("main")).accept_token(token, id(peer));
        (
            Registration {
                listener: "public".into(),
                peer: id(peer),
                identity: Arc::new(identity),
                protocol: "rutis.2".into(),
                deliver: Box::new(move |channel| {
                    let _ = sender.lock().unwrap().send(channel);
                }),
            },
            received,
        )
    }

    fn channel() -> Channel {
        struct Nothing;
        impl crate::channel::Sender for Nothing {
            fn send(&mut self, _: &[u8]) -> Result<(), crate::channel::ChannelError> {
                Ok(())
            }
        }
        impl crate::channel::Receiver for Nothing {
            fn recv(&mut self) -> Result<Option<Vec<u8>>, crate::channel::ChannelError> {
                Ok(None)
            }
        }
        impl crate::channel::Closer for Nothing {
            fn close(&self, _: &str) {}
        }
        Channel {
            sender: Box::new(Nothing),
            receiver: Box::new(Nothing),
            closer: Arc::new(Nothing),
            info: Default::default(),
        }
    }

    #[test]
    fn routes_by_verified_identity_and_refuses_everything_else() {
        let registry = Registrations::new();
        let (mac, from_mac) = registration("mac", "mac-token");
        let _mac = registry.register(&id("main"), mac).unwrap();
        let (pi, _) = registration("pi", "pi-token");
        let _pi = registry.register(&id("main"), pi).unwrap();

        let ticket = registry
            .route("public", Presented::Bearer("mac-token"))
            .unwrap();
        assert_eq!(ticket.peer, id("mac"));
        assert!(registry.hand_over(&ticket, channel()).is_ok());
        assert!(from_mac.try_recv().is_ok());

        assert_eq!(
            registry.route("public", Presented::Bearer("nobody")),
            Err(Refusal::Unauthenticated)
        );
        // Nothing registered there: nobody listens yet.
        assert_eq!(
            registry.route("other", Presented::Bearer("mac-token")),
            Err(Refusal::NotListening)
        );
    }

    /// A far end this listener's identity knows, before its link registered
    /// (a dialer quicker than the listening link): retry, not rejection.
    #[test]
    fn a_known_far_end_whose_link_has_not_registered_is_not_listened_for_yet() {
        let registry = Registrations::new();
        let (mut mac, _) = registration("mac", "mac-token");
        mac.identity = Arc::new(
            StaticIdentity::new(id("main"))
                .accept_token("mac-token", id("mac"))
                .accept_token("pi-token", id("pi")),
        );
        let _mac = registry.register(&id("main"), mac).unwrap();
        assert_eq!(
            registry.route("public", Presented::Bearer("pi-token")),
            Err(Refusal::NotListening)
        );
        assert_eq!(
            registry.route("public", Presented::Bearer("forged")),
            Err(Refusal::Unauthenticated)
        );
    }

    #[test]
    fn one_registration_per_far_end_and_the_listener_endpoint_must_match() {
        let registry = Registrations::new();
        let (first, _) = registration("mac", "a");
        let _first = registry.register(&id("main"), first).unwrap();
        let (second, _) = registration("mac", "b");
        assert!(matches!(
            registry.register(&id("main"), second),
            Err(RegistrationError::Duplicate(_))
        ));
        let (elsewhere, _) = registration("pi", "c");
        assert!(matches!(
            registry.register(&id("other"), elsewhere),
            Err(RegistrationError::Invalid(_))
        ));
    }

    #[test]
    fn a_ticket_from_before_a_revocation_cannot_hand_over_after_it() {
        let registry = Registrations::new();
        let (mac, from_mac) = registration("mac", "token");
        let handle = registry.register(&id("main"), mac).unwrap();
        let ticket = registry
            .route("public", Presented::Bearer("token"))
            .unwrap();
        drop(handle);
        assert!(registry.hand_over(&ticket, channel()).is_err());
        // A new registration of the same far end is a new generation: the
        // old ticket still cannot use it.
        let (again, _) = registration("mac", "token");
        let _again = registry.register(&id("main"), again).unwrap();
        assert!(registry.hand_over(&ticket, channel()).is_err());
        assert!(from_mac.try_recv().is_err());
        assert!(!registry.listens("nowhere"));
        assert!(registry.listens("public"));
    }
}
