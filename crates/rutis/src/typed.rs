//! Typed dependencies (draft, #50): a plugin names its dependencies once,
//! as a type, and receives them as an `apply` argument.
//!
//! [`Typed`] turns a [`TypedPlugin`] into an ordinary [`Plugin`]: its
//! `injects()` come from [`Deps::injects`], and its `apply` reads the values
//! with [`Ctx::require`] / [`Ctx::get`] before calling the typed `apply`.
//! Gating, eviction, reload and the rule that services are not visible while
//! unloading are those of every plugin; typed and untyped plugins provide to
//! and depend on each other freely.
//!
//! Only dependencies taken through `Deps` are checked at compile time; the
//! `Ctx` passed to `apply` still reads anything at runtime.

use std::sync::Arc;

use crate::ctx::Ctx;
use crate::error::CordisError;
use crate::key::TypeKey;
use crate::plugin::Plugin;
use crate::{BoxFuture, Effect};

/// A set of dependencies read from a context.
///
/// - `Arc<T>`: required; gates the plugin and is read with [`Ctx::require`].
/// - `Option<Arc<T>>`: optional; does not gate, read with [`Ctx::get`].
/// - `()` and tuples of up to eight of these.
pub trait Deps: Sized + Send + 'static {
    /// Appends the keys that gate the plugin.
    fn injects(keys: &mut Vec<TypeKey>);

    /// Reads the values; called in `apply`, after the gate has opened.
    fn resolve(ctx: &Ctx) -> Result<Self, CordisError>;
}

impl<T: Send + Sync + 'static> Deps for Arc<T> {
    fn injects(keys: &mut Vec<TypeKey>) {
        keys.push(TypeKey::of::<T>());
    }

    fn resolve(ctx: &Ctx) -> Result<Self, CordisError> {
        Ok(ctx.require::<T>()?)
    }
}

impl<T: Send + Sync + 'static> Deps for Option<Arc<T>> {
    fn injects(_: &mut Vec<TypeKey>) {}

    fn resolve(ctx: &Ctx) -> Result<Self, CordisError> {
        Ok(ctx.get::<T>())
    }
}

impl Deps for () {
    fn injects(_: &mut Vec<TypeKey>) {}

    fn resolve(_: &Ctx) -> Result<Self, CordisError> {
        Ok(())
    }
}

macro_rules! tuple_deps {
    ($($name:ident),+) => {
        impl<$($name: Deps),+> Deps for ($($name,)+) {
            fn injects(keys: &mut Vec<TypeKey>) {
                $($name::injects(keys);)+
            }

            fn resolve(ctx: &Ctx) -> Result<Self, CordisError> {
                Ok(($($name::resolve(ctx)?,)+))
            }
        }
    };
}

tuple_deps!(A);
tuple_deps!(A, B);
tuple_deps!(A, B, C);
tuple_deps!(A, B, C, D);
tuple_deps!(A, B, C, D, E);
tuple_deps!(A, B, C, D, E, F);
tuple_deps!(A, B, C, D, E, F, G);
tuple_deps!(A, B, C, D, E, F, G, H);

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

    /// Keys that gate the plugin without being passed to `apply`, such as a
    /// readiness marker. They join the keys of `Deps`.
    fn gates(&self) -> Vec<TypeKey> {
        Vec::new()
    }

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

/// A [`TypedPlugin`] as a [`Plugin`]: `ctx.plugin(Typed::new(plugin))`.
pub struct Typed<P> {
    plugin: P,
    injects: Vec<TypeKey>,
}

impl<P: TypedPlugin> Typed<P> {
    pub fn new(plugin: P) -> Self {
        let mut injects = Vec::new();
        P::Deps::injects(&mut injects);
        for key in plugin.gates() {
            if !injects.contains(&key) {
                injects.push(key);
            }
        }
        Self { plugin, injects }
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
        match P::Deps::resolve(ctx) {
            Ok(deps) => self.plugin.apply(ctx, deps),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}
