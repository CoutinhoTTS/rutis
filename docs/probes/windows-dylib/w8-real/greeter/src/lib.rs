//! Copy of tests/dylib-fixtures/greeter-v1 without the `.init_array` marker.

use rutis_sdk::rutis::{
    BoxFuture, CordisError, Ctx, Effect, FiberState, Plugin, PluginFactory, Snapshot,
};
use rutis_sdk::ConfigValue;

struct Factory;
struct Greeter;

impl Drop for Greeter {
    fn drop(&mut self) {
        if let Some(path) = std::env::var_os("RUTIS_PLUGIN_DROP_MARKER") {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open plugin drop marker");
            writeln!(file, "drop").expect("write plugin drop marker");
        }
    }
}

impl PluginFactory<ConfigValue> for Factory {
    fn name(&self) -> &str {
        "greeter"
    }
    fn build(&self, _: &ConfigValue) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(Greeter))
    }
}

impl Plugin for Greeter {
    fn name(&self) -> &str {
        "greeter"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            rutis_sdk::tokio::spawn(async {})
                .await
                .map_err(|e| CordisError::PluginFailed(e.into()))?;
            ctx.provide("hello v1".to_string())?;
            ctx.provide(Snapshot {
                generation: 1,
                state: FiberState::Active,
                error: None,
            })?;
            Ok(Effect::Done)
        })
    }
}

rutis_sdk::export_plugin! { id: "greeter", factory: Factory }
