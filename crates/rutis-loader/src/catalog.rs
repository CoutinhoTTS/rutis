//! Service names for configs and expressions.
//!
//! Configs name services by string (`isolate: { llm: true }`, `inject:
//! [llm]`); rutis keys them by type. The catalog maps one to the other, and
//! records how to test or read a service, since the kernel offers no lookup
//! by an erased key.

use std::collections::HashMap;
use std::sync::Arc;

use rutis::{Ctx, TypeKey};
use serde::Serialize;
use serde_json::Value;

use crate::LoaderError;

type Probe = Arc<dyn Fn(&Ctx) -> bool + Send + Sync>;
type Read = Arc<dyn Fn(&Ctx) -> Option<Value> + Send + Sync>;

#[derive(Clone)]
struct Service {
    key: TypeKey,
    exists: Probe,
    /// Present for services expressions may read.
    read: Option<Read>,
}

/// Service name → key, plus whether expressions may read the value.
#[derive(Clone, Default)]
pub struct ServiceCatalog {
    services: HashMap<String, Service>,
    /// Every name not registered otherwise is shared by name.
    #[cfg_attr(not(any(feature = "runtimes", feature = "peer")), allow(dead_code))]
    by_name: bool,
}

impl ServiceCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Name the service `T` registered under `TypeKey::of::<T>()`.
    pub fn register<T: ?Sized + Send + Sync + 'static>(
        &mut self,
        name: impl Into<String>,
    ) -> &mut Self {
        self.register_keyed::<T>(name, TypeKey::of::<T>())
    }

    /// Name a service registered under an explicit key (named, dynamic or
    /// instance keys).
    pub fn register_keyed<T: ?Sized + Send + Sync + 'static>(
        &mut self,
        name: impl Into<String>,
        key: TypeKey,
    ) -> &mut Self {
        let probe = key.clone();
        self.services.insert(
            name.into(),
            Service {
                key,
                exists: Arc::new(move |ctx: &Ctx| ctx.get_as::<T>(probe.clone()).is_some()),
                read: None,
            },
        );
        self
    }

    /// Name a service whose value expressions may read, serialized to JSON.
    /// Meant for small host-owned values such as startup parameters.
    pub fn readable<T: Serialize + Send + Sync + 'static>(
        &mut self,
        name: impl Into<String>,
    ) -> &mut Self {
        self.readable_keyed::<T>(name, TypeKey::of::<T>())
    }

    pub fn readable_keyed<T: Serialize + Send + Sync + 'static>(
        &mut self,
        name: impl Into<String>,
        key: TypeKey,
    ) -> &mut Self {
        let probe = key.clone();
        let reader = key.clone();
        self.services.insert(
            name.into(),
            Service {
                key,
                exists: Arc::new(move |ctx: &Ctx| ctx.get_as::<T>(probe.clone()).is_some()),
                read: Some(Arc::new(move |ctx: &Ctx| {
                    ctx.get_as::<T>(reader.clone())
                        .and_then(|value| serde_json::to_value(&*value).ok())
                })),
            },
        );
        self
    }

    /// Name a service shared across languages: a `dyn HostDispatch` at
    /// `host_key(name)`, provided by Rust or by a row of any runtime. Rows
    /// that inject a shared name wait for it in rutis; other names a
    /// JavaScript row injects are left to Cordis.
    #[cfg(all(unix, feature = "runtimes"))]
    pub fn register_shared(&mut self, name: impl Into<String>) -> &mut Self {
        let name = name.into();
        let key = rutis_bridge::session::host_key(&name);
        self.register_keyed::<dyn rutis_bridge::session::HostDispatch>(name, key)
    }

    /// Share every name not registered otherwise, as
    /// [`ServiceCatalog::register_shared`] would: a host whose services all
    /// cross between languages and nodes by name (rutis-host).
    #[cfg(any(feature = "runtimes", feature = "peer"))]
    pub fn share_by_name(&mut self) -> &mut Self {
        self.by_name = true;
        self
    }

    /// Whether `name` is shared across languages by name.
    #[cfg(all(unix, feature = "runtimes"))]
    pub fn is_shared(&self, name: &str) -> bool {
        self.key(name) == Some(rutis_bridge::session::host_key(name))
    }

    /// The key of a named service.
    pub fn key(&self, name: &str) -> Option<TypeKey> {
        match self.services.get(name) {
            Some(service) => Some(service.key.clone()),
            None => self.shared_key(name),
        }
    }

    #[cfg(any(feature = "runtimes", feature = "peer"))]
    fn shared_key(&self, name: &str) -> Option<TypeKey> {
        self.by_name.then(|| rutis_bridge::session::host_key(name))
    }

    #[cfg(not(any(feature = "runtimes", feature = "peer")))]
    fn shared_key(&self, _: &str) -> Option<TypeKey> {
        None
    }

    /// Keys for `names`, or every unknown name.
    pub(crate) fn keys<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<(String, TypeKey)>, LoaderError> {
        let mut found = Vec::new();
        let mut unknown = Vec::new();
        for name in names {
            match self.key(name) {
                Some(key) => found.push((name.to_owned(), key)),
                None => unknown.push(name.to_owned()),
            }
        }
        if unknown.is_empty() {
            Ok(found)
        } else {
            Err(LoaderError::UnknownService(unknown))
        }
    }
}

/// What an expression may see: services by catalog name, through the
/// context of the row being evaluated (so `isolate` applies).
pub struct ExprScope<'a> {
    ctx: Option<&'a Ctx>,
    catalog: &'a ServiceCatalog,
}

impl<'a> ExprScope<'a> {
    /// A scope over `ctx` (`None`: no service is available). The loader
    /// builds these itself; this is public so evaluators can be tested.
    pub fn new(ctx: Option<&'a Ctx>, catalog: &'a ServiceCatalog) -> Self {
        Self { ctx, catalog }
    }

    /// Whether the named service is currently available. Any catalog name
    /// may be tested; an unknown name is an error, not `false`.
    pub fn has(&self, name: &str) -> Result<bool, LoaderError> {
        let service = self
            .catalog
            .services
            .get(name)
            .ok_or_else(|| LoaderError::UnknownService(vec![name.to_owned()]))?;
        Ok(self.ctx.is_some_and(|ctx| (service.exists)(ctx)))
    }

    /// The value of a readable service, `None` while it is absent. Services
    /// not registered as readable cannot be read.
    pub fn read(&self, name: &str) -> Result<Option<Value>, LoaderError> {
        let service = self
            .catalog
            .services
            .get(name)
            .ok_or_else(|| LoaderError::UnknownService(vec![name.to_owned()]))?;
        let read = service
            .read
            .as_ref()
            .ok_or_else(|| LoaderError::NotReadable(name.to_owned()))?;
        Ok(self.ctx.and_then(|ctx| read(ctx)))
    }
}

/// Evaluates expression nodes (`{ "__jsExpr": "<source>" }`) in rows.
/// The loader ships no implementation; dsh's lives in rutis-dsh.
///
/// Called with the loader's state lock held: it must not call back into the
/// loader.
pub trait Expressions: Send + Sync + 'static {
    fn evaluate(&self, expr: &str, scope: &ExprScope<'_>) -> Result<Value, LoaderError>;
}
