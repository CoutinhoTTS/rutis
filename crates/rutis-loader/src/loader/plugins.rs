//! Kernel glue: the entry factory, group plugin and loader plugin.

use std::sync::{Arc, Mutex, Weak};

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, PluginFactory, TypeKey};
use serde_json::Value;

use crate::resolver::{Resolved, Resolver};

use super::{Inner, Loader, LoaderOptions, State};

/// One generation's module and evaluated config.
#[derive(Clone)]
pub(super) struct EntryConfig {
    pub(super) resolved: Arc<Resolved>,
    pub(super) value: Value,
}

pub(super) struct EntryFactory {
    pub(super) name: String,
    pub(super) injects: Vec<TypeKey>,
}

impl PluginFactory<EntryConfig> for EntryFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn validate_config(&self, config: &EntryConfig) -> Result<(), CordisError> {
        config.resolved.factory.validate_config(&config.value)
    }

    fn build(&self, config: &EntryConfig) -> Result<Box<dyn Plugin>, CordisError> {
        config.resolved.factory.build(&config.value)
    }
}

/// A group row: spawns its children in its own context, so disabling the
/// group unloads them through the kernel's cascade.
pub(super) struct GroupPlugin {
    pub(super) inner: Weak<Inner>,
    pub(super) id: String,
    /// Keys from the row's `inject`.
    pub(super) injects: Vec<TypeKey>,
    /// The token of the spawn that created this plugin.
    pub(super) token: u64,
}

impl Plugin for GroupPlugin {
    fn name(&self) -> &str {
        &self.id
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let Some(inner) = self.inner.upgrade() else {
                return Ok(Effect::Done);
            };
            let token = self.token;
            inner.attach(Some(self.id.clone()), token, ctx);
            let weak = self.inner.clone();
            let id = self.id.clone();
            Ok(Effect::Disposer(Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.detach(Some(id), token);
                }
                Ok(())
            })))
        })
    }
}

/// Mounts a [`Loader`] and provides it as a service.
pub struct LoaderPlugin {
    loader: Loader,
}

impl LoaderPlugin {
    pub fn new(resolver: impl Resolver, options: LoaderOptions) -> Self {
        Self {
            loader: Loader {
                inner: Arc::new(Inner {
                    resolver: Arc::new(resolver),
                    persist: options.persist,
                    catalog: options.catalog,
                    expressions: options.expressions,
                    op: tokio::sync::Mutex::new(()),
                    state: Mutex::new(State::default()),
                }),
            },
        }
    }

    pub fn handle(&self) -> Loader {
        self.loader.clone()
    }
}

impl Plugin for LoaderPlugin {
    fn name(&self) -> &str {
        "rutis-loader"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            // The service is released with the fiber.
            ctx.provide(self.loader.clone())?;
            let token = self.loader.inner.next_token();
            self.loader.inner.attach(None, token, ctx);
            let weak = Arc::downgrade(&self.loader.inner);
            Ok(Effect::Disposer(Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.detach(None, token);
                }
                Ok(())
            })))
        })
    }
}
