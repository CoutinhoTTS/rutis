//! The example of docs/migration-0.6.0-to-0.6.1.md, kept compiling and
//! running: rows from a layer, then edits through the editable layer.

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, PluginFactory};
use rutis_loader::{Builtins, Editable, Layer, LoaderOptions, LoaderPlugin, Patch, Version};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, schemars::JsonSchema)]
struct MyConfig {
    level: u32,
}

struct MyPlugin;

impl Plugin for MyPlugin {
    fn name(&self) -> &str {
        "my-plugin"
    }

    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Ok(Effect::Done) })
    }
}

struct MyFactory;

impl PluginFactory<MyConfig> for MyFactory {
    fn name(&self) -> &str {
        "my-plugin"
    }

    fn build(&self, _config: &MyConfig) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(MyPlugin))
    }
}

#[tokio::test]
async fn the_migration_example_runs() -> Result<(), Box<dyn std::error::Error>> {
    let root = Ctx::root()?;

    let mut builtins = Builtins::new();
    builtins.register::<MyConfig, _>("my-plugin", MyFactory);
    let plugin = LoaderPlugin::new(builtins, LoaderOptions::default());
    let loader = plugin.handle();
    root.plugin(plugin).await?;

    let app: Vec<Patch> = serde_json::from_value(json!([
        { "insert": [{ "id": "main", "name": "my-plugin", "config": { "level": 1 } }] }
    ]))?;
    loader
        .reconcile(
            vec![Layer::new("app", app), Layer::new("user", Vec::new())],
            Some(Editable::new("user", Version::default())),
        )
        .await?;
    loader.update("main", json!({ "level": 2 })).await?;
    loader.set_disabled("main", true).await?;

    assert_eq!(
        loader.layers()[1].patches.len(),
        1,
        "the edit went to the user layer"
    );
    assert_eq!(
        loader.layers()[0].patches.len(),
        1,
        "the app layer is untouched"
    );
    Ok(())
}
