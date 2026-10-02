//! Typed dependencies (draft, #50): a plugin names its dependencies once,
//! as a type, and receives them as an `apply` argument.
//!
//! One description, the [`Deps`] type plus its runtime [`Deps::Keys`],
//! produces both the gate declaration and the typed reads. [`Typed`] turns
//! a [`TypedPlugin`] into an ordinary [`Plugin`] and [`TypedFactory`] a
//! [`TypedPluginFactory`] into an ordinary [`PluginFactory`]; gating,
//! eviction, reload and the rule that services are not visible while
//! unloading are those of every plugin, and typed and untyped plugins
//! provide to and depend on each other freely.
//!
//! Only dependencies taken through `Deps` are checked at compile time; the
//! `Ctx` passed to `apply` still reads anything at runtime.

use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Arc;

use crate::ctx::Ctx;
use crate::error::{CordisError, ServiceReadError, ServiceReadFailure};
use crate::key::{InstanceId, Key, TypeKey};
use crate::plugin::{Plugin, PluginFactory};
use crate::{BoxFuture, Effect};

/// A set of dependencies read from a context.
///
/// | Type | Keys | Gates | Value |
/// |---|---|---|---|
/// | `Arc<T>` | `()` | `TypeKey::of::<T>()` | the service |
/// | `Option<Arc<T>>` | `()` | no | the service if visible at load |
/// | [`Keyed<T>`] | [`DepKey<T>`] | that key | the service |
/// | `Option<Keyed<T>>` | [`DepKey<T>`] | no | the service if visible at load |
/// | [`Gate<T>`] | `()` | `TypeKey::of::<T>()` | nothing |
/// | [`KeyedGate<T>`] | [`DepKey<T>`] | that key | nothing |
/// | `()`, tuples of up to eight | tuple of the members' keys | all | each |
///
/// `T` may be unsized (`Arc<dyn Trait>`). An optional dependency is a
/// snapshot taken when `apply` starts: it does not gate, and its arrival or
/// departure does not reload the plugin. A required dependency withdrawn
/// after the gate opened but before `apply` reads it returns the plugin to
/// Pending.
pub trait Deps: Sized + Send + 'static {
    /// The runtime part of the description (named or instance keys); `()`
    /// when every key follows from the type.
    type Keys: Clone + Send + Sync + 'static;

    /// Appends the keys that gate the plugin.
    fn injects(keys: &Self::Keys, out: &mut Vec<TypeKey>);

    /// Reads the values; called when `apply` starts, after the gate opened.
    fn resolve(keys: &Self::Keys, ctx: &Ctx) -> Result<Self, CordisError>;
}

/// A service key whose value type is `T`: every constructor names `T`, so
/// a key cannot point at a service of another type.
pub struct DepKey<T: ?Sized + 'static> {
    key: TypeKey,
    _marker: PhantomData<fn() -> T>,
}

impl<T: ?Sized + 'static> DepKey<T> {
    /// `TypeKey::of::<T>()`.
    pub fn of() -> Self {
        Self::wrap(TypeKey::of::<T>())
    }

    /// A named service of type `T`.
    pub fn named(name: &'static str) -> Self {
        Self::wrap(TypeKey::keyed::<T>(name))
    }

    /// A service of type `T` whose name is chosen at runtime.
    pub fn dynamic(name: impl Into<Arc<str>>) -> Self {
        Self::wrap(TypeKey::keyed_dynamic::<T>(name))
    }

    /// This key on instance `id`.
    pub fn instance(self, id: InstanceId) -> Self {
        Self::wrap(self.key.with_instance(id))
    }

    pub fn key(&self) -> &TypeKey {
        &self.key
    }

    fn wrap(key: TypeKey) -> Self {
        Self {
            key,
            _marker: PhantomData,
        }
    }
}

impl<T: ?Sized + 'static> From<Key<T>> for DepKey<T> {
    fn from(key: Key<T>) -> Self {
        Self::wrap(key.into())
    }
}

/// `DepKey::of()`.
impl<T: ?Sized + 'static> Default for DepKey<T> {
    fn default() -> Self {
        Self::of()
    }
}

impl<T: ?Sized + 'static> Clone for DepKey<T> {
    fn clone(&self) -> Self {
        Self::wrap(self.key.clone())
    }
}

impl<T: ?Sized + 'static> std::fmt::Debug for DepKey<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.key.fmt(f)
    }
}

/// A required service found under a [`DepKey`].
pub struct Keyed<T: ?Sized>(pub Arc<T>);

impl<T: ?Sized> Deref for Keyed<T> {
    type Target = Arc<T>;

    fn deref(&self) -> &Arc<T> {
        &self.0
    }
}

/// Gates the plugin on `TypeKey::of::<T>()` without passing the service.
pub struct Gate<T: ?Sized>(PhantomData<fn() -> T>);

/// Gates the plugin on a [`DepKey`] without passing the service.
pub struct KeyedGate<T: ?Sized>(PhantomData<fn() -> T>);

/// A required read: a dependency withdrawn after the gate opened is
/// reported as such, so the load returns to Pending.
fn require<T: ?Sized + Send + Sync + 'static>(
    ctx: &Ctx,
    key: &TypeKey,
) -> Result<Arc<T>, CordisError> {
    ctx.require_as::<T>(key.clone())
        .map_err(|error: ServiceReadError| match error.reason {
            ServiceReadFailure::Unavailable(_) => {
                CordisError::InjectUnsatisfied(vec![error.key.describe()])
            }
            _ => error.into(),
        })
}

impl<T: ?Sized + Send + Sync + 'static> Deps for Arc<T> {
    type Keys = ();

    fn injects(_: &(), out: &mut Vec<TypeKey>) {
        out.push(TypeKey::of::<T>());
    }

    fn resolve(_: &(), ctx: &Ctx) -> Result<Self, CordisError> {
        require(ctx, &TypeKey::of::<T>())
    }
}

impl<T: ?Sized + Send + Sync + 'static> Deps for Option<Arc<T>> {
    type Keys = ();

    fn injects(_: &(), _: &mut Vec<TypeKey>) {}

    fn resolve(_: &(), ctx: &Ctx) -> Result<Self, CordisError> {
        Ok(ctx.get_as::<T>(TypeKey::of::<T>()))
    }
}

impl<T: ?Sized + Send + Sync + 'static> Deps for Keyed<T> {
    type Keys = DepKey<T>;

    fn injects(keys: &DepKey<T>, out: &mut Vec<TypeKey>) {
        out.push(keys.key.clone());
    }

    fn resolve(keys: &DepKey<T>, ctx: &Ctx) -> Result<Self, CordisError> {
        require(ctx, &keys.key).map(Keyed)
    }
}

impl<T: ?Sized + Send + Sync + 'static> Deps for Option<Keyed<T>> {
    type Keys = DepKey<T>;

    fn injects(_: &DepKey<T>, _: &mut Vec<TypeKey>) {}

    fn resolve(keys: &DepKey<T>, ctx: &Ctx) -> Result<Self, CordisError> {
        Ok(ctx.get_as::<T>(keys.key.clone()).map(Keyed))
    }
}

impl<T: ?Sized + 'static> Deps for Gate<T> {
    type Keys = ();

    fn injects(_: &(), out: &mut Vec<TypeKey>) {
        out.push(TypeKey::of::<T>());
    }

    fn resolve(_: &(), _: &Ctx) -> Result<Self, CordisError> {
        Ok(Gate(PhantomData))
    }
}

impl<T: ?Sized + 'static> Deps for KeyedGate<T> {
    type Keys = DepKey<T>;

    fn injects(keys: &DepKey<T>, out: &mut Vec<TypeKey>) {
        out.push(keys.key.clone());
    }

    fn resolve(_: &DepKey<T>, _: &Ctx) -> Result<Self, CordisError> {
        Ok(KeyedGate(PhantomData))
    }
}

impl Deps for () {
    type Keys = ();

    fn injects(_: &(), _: &mut Vec<TypeKey>) {}

    fn resolve(_: &(), _: &Ctx) -> Result<Self, CordisError> {
        Ok(())
    }
}

macro_rules! tuple_deps {
    ($($name:ident $index:tt),+) => {
        impl<$($name: Deps),+> Deps for ($($name,)+) {
            type Keys = ($($name::Keys,)+);

            fn injects(keys: &Self::Keys, out: &mut Vec<TypeKey>) {
                $($name::injects(&keys.$index, out);)+
            }

            fn resolve(keys: &Self::Keys, ctx: &Ctx) -> Result<Self, CordisError> {
                Ok(($($name::resolve(&keys.$index, ctx)?,)+))
            }
        }
    };
}

tuple_deps!(A 0);
tuple_deps!(A 0, B 1);
tuple_deps!(A 0, B 1, C 2);
tuple_deps!(A 0, B 1, C 2, D 3);
tuple_deps!(A 0, B 1, C 2, D 3, E 4);
tuple_deps!(A 0, B 1, C 2, D 3, E 4, F 5);
tuple_deps!(A 0, B 1, C 2, D 3, E 4, F 5, G 6);
tuple_deps!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);

/// The gate declaration of `D` under `keys`, each key once.
fn declaration<D: Deps>(keys: &D::Keys) -> Vec<TypeKey> {
    let mut all = Vec::new();
    D::injects(keys, &mut all);
    let mut injects = Vec::with_capacity(all.len());
    for key in all {
        if !injects.contains(&key) {
            injects.push(key);
        }
    }
    injects
}

/// A plugin whose dependencies are a type. Mount it with [`Typed`].
///
/// ```
/// use std::sync::Arc;
/// use rutis::{BoxFuture, CordisError, Ctx, Effect, Typed, TypedPlugin};
///
/// struct Llm;
/// struct Logger;
/// struct Chat;
///
/// impl TypedPlugin for Chat {
///     // Starts once `Llm` is available; `Logger` is passed when present.
///     type Deps = (Arc<Llm>, Option<Arc<Logger>>);
///
///     fn name(&self) -> &str {
///         "chat"
///     }
///
///     fn apply<'a>(
///         &'a self,
///         ctx: &'a Ctx,
///         (llm, logger): Self::Deps,
///     ) -> BoxFuture<'a, Result<Effect, CordisError>> {
///         Box::pin(async { Ok(Effect::Done) })
///     }
/// }
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let ctx = Ctx::root().unwrap();
/// ctx.provide(Llm).unwrap();
/// let view = ctx.plugin(Typed::new(Chat));
/// (&view).await.unwrap();
/// # }
/// ```
///
/// What `apply` takes is what gates it, so they cannot disagree:
///
/// ```compile_fail
/// # use std::sync::Arc;
/// # use rutis::{BoxFuture, CordisError, Ctx, Effect, TypedPlugin};
/// # struct Llm;
/// # struct Embedder;
/// # struct Chat;
/// impl TypedPlugin for Chat {
///     type Deps = (Arc<Llm>,);
///     # fn name(&self) -> &str { "chat" }
///     fn apply<'a>(
///         &'a self,
///         ctx: &'a Ctx,
///         (embedder,): (Arc<Embedder>,), // declared Llm, takes Embedder
///     ) -> BoxFuture<'a, Result<Effect, CordisError>> {
///         Box::pin(async { Ok(Effect::Done) })
///     }
/// }
/// ```
pub trait TypedPlugin: Send + Sync + 'static {
    /// What `apply` receives; see [`Deps`].
    type Deps: Deps;

    /// See [`Plugin::name`].
    fn name(&self) -> &str;

    /// See [`Plugin::validate`].
    fn validate(&self) -> Result<(), CordisError> {
        Ok(())
    }

    /// See [`Plugin::apply`]; `deps` were read from `ctx` just before.
    fn apply<'a>(
        &'a self,
        ctx: &'a Ctx,
        deps: Self::Deps,
    ) -> BoxFuture<'a, Result<Effect, CordisError>>;
}

type KeysOf<P> = <<P as TypedPlugin>::Deps as Deps>::Keys;

/// A [`TypedPlugin`] as a [`Plugin`]: `ctx.plugin(Typed::new(plugin))`.
pub struct Typed<P: TypedPlugin> {
    plugin: P,
    keys: KeysOf<P>,
    injects: Vec<TypeKey>,
}

impl<P: TypedPlugin> Typed<P> {
    /// For dependencies whose keys follow from their types.
    pub fn new(plugin: P) -> Self
    where
        KeysOf<P>: Default,
    {
        Self::with_keys(plugin, Default::default())
    }

    /// With the runtime keys of the dependencies, chosen when mounting
    /// (a name, an instance).
    pub fn with_keys(plugin: P, keys: KeysOf<P>) -> Self {
        let injects = declaration::<P::Deps>(&keys);
        Self {
            plugin,
            keys,
            injects,
        }
    }

    pub fn inner(&self) -> &P {
        &self.plugin
    }
}

impl<P: TypedPlugin> Plugin for Typed<P> {
    fn name(&self) -> &str {
        self.plugin.name()
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn validate(&self) -> Result<(), CordisError> {
        self.plugin.validate()
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        match P::Deps::resolve(&self.keys, ctx) {
            Ok(deps) => self.plugin.apply(ctx, deps),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

/// A factory of typed plugins (configuration hot update). Mount it with
/// [`TypedFactory`]; the declaration comes from `Plugin::Deps`, as for
/// [`Typed`], and every built plugin reads with the same keys.
pub trait TypedPluginFactory<C: Send + Sync + 'static>: Send + Sync + 'static {
    type Plugin: TypedPlugin;

    /// See [`PluginFactory::name`].
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }

    /// See [`PluginFactory::validate_config`].
    fn validate_config(&self, _config: &C) -> Result<(), CordisError> {
        Ok(())
    }

    /// See [`PluginFactory::build`].
    fn build(&self, config: &C) -> Result<Self::Plugin, CordisError>;
}

/// A [`TypedPluginFactory`] as a [`PluginFactory`]:
/// `ctx.plugin_with(TypedFactory::new(factory), config)`.
pub struct TypedFactory<F, C>
where
    F: TypedPluginFactory<C>,
    C: Send + Sync + 'static,
{
    factory: F,
    keys: KeysOf<F::Plugin>,
    injects: Vec<TypeKey>,
    _config: PhantomData<fn() -> C>,
}

impl<F, C> TypedFactory<F, C>
where
    F: TypedPluginFactory<C>,
    C: Send + Sync + 'static,
{
    pub fn new(factory: F) -> Self
    where
        KeysOf<F::Plugin>: Default,
    {
        Self::with_keys(factory, Default::default())
    }

    pub fn with_keys(factory: F, keys: KeysOf<F::Plugin>) -> Self {
        let injects = declaration::<<F::Plugin as TypedPlugin>::Deps>(&keys);
        Self {
            factory,
            keys,
            injects,
            _config: PhantomData,
        }
    }
}

impl<F, C> PluginFactory<C> for TypedFactory<F, C>
where
    F: TypedPluginFactory<C>,
    C: Send + Sync + 'static,
{
    fn name(&self) -> &str {
        self.factory.name()
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn validate_config(&self, config: &C) -> Result<(), CordisError> {
        self.factory.validate_config(config)
    }

    fn build(&self, config: &C) -> Result<Box<dyn Plugin>, CordisError> {
        let plugin = self.factory.build(config)?;
        Ok(Box::new(Typed::with_keys(plugin, self.keys.clone())))
    }
}
