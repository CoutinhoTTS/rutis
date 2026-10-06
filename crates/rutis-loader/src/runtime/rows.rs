//! The second stage of a runtime: rows start only once their declarations
//! are complete.
//!
//! A row resolved before its runtime was running has no schema and none of
//! the dependencies its plugin declares, since only the runtime can read
//! them. Were rows to start as soon as the runtime does, such a row could
//! run before a service it injects is there. So the runtime provides
//! `Runtime` first; this plugin then resolves those rows again and
//! only afterwards provides [`RuntimeRows`], which every row depends
//! on. Rows waiting for it do not start while they are reloaded.

use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, TypeKey};
use rutis_bridge::runtime::Runtime;

use super::RuntimeResolver;
use crate::Loader;

/// The service rows of a runtime depend on: the runtime, once the
/// rows' declarations are complete.
pub struct RuntimeRows {
    runtime: Arc<Runtime>,
}

impl RuntimeRows {
    /// The key for the rows of the runtime named `name`.
    pub fn key(name: &str) -> TypeKey {
        TypeKey::keyed_dynamic::<RuntimeRows>(name.to_owned())
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }
}

/// Mount it after the runtime and the loader, with the resolver the loader
/// uses (`Chain::with_shared`).
pub struct RuntimeRowsPlugin {
    name: String,
    resolver: Arc<RuntimeResolver>,
    injects: [TypeKey; 2],
}

impl RuntimeRowsPlugin {
    pub fn new(resolver: Arc<RuntimeResolver>) -> Self {
        let name = resolver.runtime_name().to_owned();
        Self {
            injects: [Runtime::key(&name), TypeKey::of::<Loader>()],
            name,
            resolver,
        }
    }
}

impl Plugin for RuntimeRowsPlugin {
    fn name(&self) -> &str {
        "runtime-rows"
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let runtime = ctx.require_as::<Runtime>(Runtime::key(&self.name))?;
            let key = RuntimeRows::key(&self.name);
            let loader = ctx.require::<Loader>()?;
            let stale = self.resolver.take_stale();
            let rows: Vec<String> = loader
                .entries()
                .into_iter()
                .filter(|entry| {
                    entry.options["name"]
                        .as_str()
                        .is_some_and(|name| stale.contains(name))
                })
                .map(|entry| entry.id)
                .collect();
            let owner = ctx.clone();
            // In a task, not in apply: reloading takes the loader's lock,
            // which a reconcile that (re)started this plugin may hold.
            let refresh = tokio::spawn(async move {
                for id in rows {
                    // A row that fails to resolve reports it in its status;
                    // the others still start.
                    if let Err(error) = loader.refresh(&id).await {
                        eprintln!("rutis-loader: cannot resolve row {id} again: {error}");
                    }
                }
                if let Err(error) = owner.provide_as(key, Arc::new(RuntimeRows { runtime })) {
                    // Disposed meanwhile: nothing to provide to.
                    if !owner.cancellation_token().is_cancelled() {
                        eprintln!("rutis-loader: cannot release the rows: {error}");
                    }
                }
            });
            let refresh = Arc::new(Mutex::new(Some(refresh)));
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    if let Some(refresh) = refresh.lock().unwrap().take() {
                        refresh.abort();
                    }
                    Ok(())
                }))
            })?;
            Ok(Effect::Done)
        })
    }
}
