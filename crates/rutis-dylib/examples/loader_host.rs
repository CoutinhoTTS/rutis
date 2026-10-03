//! Release-only integration fixture for `DylibResolver`, run by
//! tools/test-dylib.sh after greeter_host: dylib bundles as rutis-loader
//! rows, swapped in place on reload and respawned when their identity
//! changes.
#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;
    use std::sync::Arc;

    use rutis::{Ctx, FiberState};
    use rutis_dylib::{DylibResolver, Loader};
    use rutis_loader::{EntryStatus, Layer, LoaderError, LoaderOptions, LoaderPlugin, Patch};

    fn replace(dir: &Path, from: &Path) -> std::io::Result<()> {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            std::fs::copy(entry.path(), dir.join(entry.file_name()))?;
        }
        Ok(())
    }

    fn state(loader: &rutis_loader::Loader, id: &str) -> Option<FiberState> {
        match loader.get(id)?.status {
            EntryStatus::Running(s) => Some(s.state),
            _ => None,
        }
    }

    #[tokio::main]
    pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
        let base =
            std::path::PathBuf::from(std::env::args().nth(1).ok_or("missing base directory")?);
        let expected = option_env!("RUTIS_SDK_ARTIFACT_SHA256")
            .ok_or("example host is not bound to an SDK artifact")?;
        let dylibs = Arc::new(Loader::new(
            expected,
            base.join("loader-cache"),
            Default::default(),
            4,
        )?);
        let current = base.join("current");
        replace(&current, &base.join("v1"))?;

        let root = Ctx::root()?;
        // SAFETY: the bundles under `base` are this test's own fixtures.
        let resolver = unsafe { DylibResolver::new(dylibs.clone(), &base) };
        let plugin = LoaderPlugin::new(resolver, LoaderOptions::default());
        let loader = plugin.handle();
        root.plugin(plugin).await?;
        let rows: Vec<Patch> = rutis_sdk::serde_json::from_value(rutis_sdk::serde_json::json!([
            { "insert": [
                { "id": "greeter", "name": "dylib:current" },
                { "id": "missing", "name": "dylib:nope" }
            ] }
        ]))?;
        let report = loader
            .reconcile(vec![Layer::new("rows", rows)], None)
            .await?;
        assert_eq!(report.failures.len(), 1, "{report:?}");
        assert!(matches!(
            loader.get("missing").unwrap().status,
            EntryStatus::Unresolved(LoaderError::Resolve { .. })
        ));
        assert_eq!(state(&loader, "greeter"), Some(FiberState::Active));
        assert_eq!(
            root.get::<String>().as_deref().map(String::as_str),
            Some("hello v1")
        );
        let entry = loader.get("greeter").unwrap();
        assert!(entry.schema.is_none());
        assert_eq!(entry.meta["version"], "1.0.0");
        let first = entry.plugin;

        // Same library again: nothing changes.
        loader.reload("greeter").await?;
        assert_eq!(loader.get("greeter").unwrap().plugin, first);

        // A new version under the same name: swapped in place.
        replace(&current, &base.join("v2"))?;
        loader.reload("greeter").await?;
        let entry = loader.get("greeter").unwrap();
        assert_eq!(entry.plugin, first, "same identity: in place");
        assert_eq!(entry.meta["version"], "2.0.0");
        assert_eq!(
            entry.schema.unwrap()["properties"]["greeting"]["type"],
            "string"
        );
        assert_eq!(
            root.get::<String>().as_deref().map(String::as_str),
            Some("hello v2")
        );

        // Another identity: respawned.
        replace(&current, &base.join("changed-identity"))?;
        loader.reload("greeter").await?;
        let entry = loader.get("greeter").unwrap();
        assert_ne!(entry.plugin, first, "changed identity: respawned");

        root.shutdown().await?;
        println!("dylib loader rows passed");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {}
