//! Module name → plugin factory.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Plugin, PluginFactory, TypeKey};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::LoaderError;

/// What a name resolves to.
pub struct Resolved {
    pub factory: Arc<dyn PluginFactory<Value>>,
    /// JSON Schema of the config, for forms and comparison only; the
    /// factory's `validate_config` stays the authority.
    pub schema: Option<Value>,
    /// Diagnostics: version, source path, hash and the like.
    pub meta: Value,
    /// The plugin handles its row's `isolate` and `inject` itself (they
    /// name services of another runtime, as for JavaScript rows): the loader
    /// neither resolves them through its catalog nor applies them, and the
    /// plugin reads them with `Loader::row`.
    pub foreign_scope: bool,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("factory", &self.factory.name())
            .field("schema", &self.schema.is_some())
            .field("meta", &self.meta)
            .finish()
    }
}

pub trait Resolver: Send + Sync + 'static {
    /// Resolve a module name. Return [`LoaderError::NotFound`] for names this
    /// resolver does not handle, so a [`Chain`] can try the next one.
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>>;
}

/// Plugins compiled into the host, by exact name. Any name works, including
/// npm package names, so a Rust reimplementation can replace a JavaScript
/// plugin without touching configs.
#[derive(Default)]
pub struct Builtins {
    entries: HashMap<String, Arc<Resolved>>,
}

impl Builtins {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a factory whose config deserializes from JSON; the schema is
    /// generated from `C`. A `null` config (the row has none) is decoded as
    /// `null` first, then as `{}`.
    pub fn register<C, F>(&mut self, name: impl Into<String>, factory: F) -> &mut Self
    where
        C: DeserializeOwned + JsonSchema + Send + Sync + 'static,
        F: PluginFactory<C>,
    {
        let schema = serde_json::to_value(schemars::schema_for!(C)).ok();
        self.insert(name, Arc::new(Json::<C, F>::new(factory)), schema)
    }

    /// Register a closure for a config type without a schema.
    pub fn register_fn<C, B>(&mut self, name: impl Into<String>, build: B) -> &mut Self
    where
        C: DeserializeOwned + Send + Sync + 'static,
        B: Fn(&C) -> Result<Box<dyn Plugin>, CordisError> + Send + Sync + 'static,
    {
        let name = name.into();
        let factory = Closure {
            name: name.clone(),
            build,
        };
        self.insert(name, Arc::new(Json::<C, _>::new(factory)), None)
    }

    /// Register a factory that takes the raw JSON config.
    pub fn register_raw(
        &mut self,
        name: impl Into<String>,
        factory: impl PluginFactory<Value>,
        schema: Option<Value>,
    ) -> &mut Self {
        self.insert(name, Arc::new(factory), schema)
    }

    fn insert(
        &mut self,
        name: impl Into<String>,
        factory: Arc<dyn PluginFactory<Value>>,
        schema: Option<Value>,
    ) -> &mut Self {
        let name = name.into();
        let meta = serde_json::json!({ "source": "builtin" });
        self.entries.insert(
            name,
            Arc::new(Resolved {
                factory,
                schema,
                meta,
                foreign_scope: false,
            }),
        );
        self
    }
}

impl Resolver for Builtins {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        let found = self.entries.get(name).cloned();
        Box::pin(async move {
            found.ok_or_else(|| LoaderError::NotFound {
                name: name.to_owned(),
            })
        })
    }
}

/// Try resolvers in order; the first that does not answer `NotFound` wins.
#[derive(Default)]
pub struct Chain {
    resolvers: Vec<Arc<dyn Resolver>>,
}

impl Chain {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, resolver: impl Resolver) -> Self {
        self.resolvers.push(Arc::new(resolver));
        self
    }

    /// Add a resolver that something else holds too, such as an
    /// `RuntimeResolver` shared with its `RuntimeRowsPlugin`.
    pub fn with_shared(mut self, resolver: Arc<dyn Resolver>) -> Self {
        self.resolvers.push(resolver);
        self
    }
}

impl Resolver for Chain {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        Box::pin(async move {
            for resolver in &self.resolvers {
                match resolver.resolve(name).await {
                    Err(LoaderError::NotFound { .. }) => continue,
                    other => return other,
                }
            }
            Err(LoaderError::NotFound {
                name: name.to_owned(),
            })
        })
    }
}

/// Adapts a `PluginFactory<C>` to JSON config.
struct Json<C, F> {
    inner: F,
    _config: PhantomData<fn() -> C>,
}

impl<C, F> Json<C, F> {
    fn new(inner: F) -> Self {
        Self {
            inner,
            _config: PhantomData,
        }
    }
}

fn decode<C: DeserializeOwned>(value: &Value) -> Result<C, CordisError> {
    let first = serde_json::from_value::<C>(value.clone());
    match (first, value) {
        (Ok(config), _) => Ok(config),
        (Err(_), Value::Null) => serde_json::from_value::<C>(Value::Object(Default::default()))
            .map_err(|e| CordisError::Validation {
                issues: vec![e.to_string()],
            }),
        (Err(e), _) => Err(CordisError::Validation {
            issues: vec![e.to_string()],
        }),
    }
}

impl<C, F> PluginFactory<Value> for Json<C, F>
where
    C: DeserializeOwned + Send + Sync + 'static,
    F: PluginFactory<C>,
{
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn injects(&self) -> &[TypeKey] {
        self.inner.injects()
    }

    fn validate_config(&self, config: &Value) -> Result<(), CordisError> {
        self.inner.validate_config(&decode::<C>(config)?)
    }

    fn build(&self, config: &Value) -> Result<Box<dyn Plugin>, CordisError> {
        self.inner.build(&decode::<C>(config)?)
    }
}

struct Closure<B> {
    name: String,
    build: B,
}

impl<C, B> PluginFactory<C> for Closure<B>
where
    C: Send + Sync + 'static,
    B: Fn(&C) -> Result<Box<dyn Plugin>, CordisError> + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn build(&self, config: &C) -> Result<Box<dyn Plugin>, CordisError> {
        (self.build)(config)
    }
}
