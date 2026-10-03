#![cfg(feature = "export")]

use rutis_sdk::rutis::{
    BoxFuture, CordisError, Ctx, Effect, FiberState, Plugin, PluginFactory, Snapshot, TypeKey,
};
use rutis_sdk::ConfigValue;

struct Factory;
struct Greeter;

impl PluginFactory<ConfigValue> for Factory {
    fn name(&self) -> &str {
        #[cfg(feature = "changed_identity")]
        return "other-greeter";
        #[cfg(not(feature = "changed_identity"))]
        "greeter"
    }
    fn injects(&self) -> &[TypeKey] {
        #[cfg(feature = "changed_identity")]
        return std::sync::OnceLock::get_or_init(&INJECTS, || vec![TypeKey::of::<u8>()]);
        #[cfg(not(feature = "changed_identity"))]
        &[]
    }
    fn build(&self, _: &ConfigValue) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(Greeter))
    }
}
#[cfg(feature = "changed_identity")]
static INJECTS: std::sync::OnceLock<Vec<TypeKey>> = std::sync::OnceLock::new();
impl Plugin for Greeter {
    fn name(&self) -> &str {
        "greeter"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            rutis_sdk::tokio::spawn(async {})
                .await
                .map_err(|e| CordisError::PluginFailed(e.into()))?;
            ctx.provide("hello v2".to_string())?;
            ctx.provide(Snapshot {
                generation: 2,
                state: FiberState::Active,
                error: None,
            })?;
            Ok(Effect::Done)
        })
    }
}
#[cfg(all(
    feature = "export",
    not(any(feature = "changed_identity", feature = "fail_once"))
))]
rutis_sdk::export_plugin! {
    id: "greeter",
    factory: Factory,
    schema: rutis_sdk::serde_json::json!({ "type": "object", "properties": { "greeting": { "type": "string" } } }),
}

#[cfg(all(
    feature = "export",
    feature = "changed_identity",
    not(feature = "fail_once")
))]
rutis_sdk::export_plugin! { id: "other-greeter", factory: Factory }

#[cfg(all(
    feature = "export",
    feature = "fail_once",
    not(feature = "changed_identity")
))]
rutis_sdk::export_plugin! { id: "greeter", factory: factory_once() }

#[cfg(feature = "fail_once")]
fn factory_once() -> Factory {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    if ATTEMPTS.fetch_add(1, Ordering::SeqCst) == 0 {
        panic!("first factory entry attempt fails");
    }
    Factory
}
