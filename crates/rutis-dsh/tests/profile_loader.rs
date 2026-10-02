//! A rutis-loader running a dsh profile from disk: expressions, edits
//! written back to the user layer with comments kept, upper-layer
//! overrides, two writers, and hot reload.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberState, Plugin};
use rutis_dsh::profile::{self, watch, ProfileContext, ProfileContextService};
use rutis_loader::{Builtins, EntryStatus, Loader, LoaderError, LoaderPlugin, NewEntry};
use serde_json::{json, Value};

type Log = Arc<Mutex<Vec<String>>>;

struct Echo {
    id: String,
    config: Value,
    log: Log,
}

impl Plugin for Echo {
    fn name(&self) -> &str {
        &self.id
    }

    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        self.log
            .lock()
            .unwrap()
            .push(format!("apply {}", self.config));
        Box::pin(async { Ok(Effect::Done) })
    }
}

fn builtins(log: &Log) -> Builtins {
    let mut builtins = Builtins::new();
    let log = log.clone();
    builtins.register_fn::<Value, _>("echo", move |config: &Value| {
        Ok(Box::new(Echo {
            id: "echo".into(),
            config: config.clone(),
            log: log.clone(),
        }))
    });
    builtins
}

const USER: &str = "\
# Your patch layer.

# the main row
- insert:
    - id: main
      name: echo
      config:
        level: 1
    - id: gated
      name: echo
      disabled: !!js \"!ctx.get('profileContext')\"
      config:
        dir: !!js ctx.profileContext.dir

# keep this comment
- id: from-base
  config:
    level: 2
";

fn setup() -> (tempfile::TempDir, ProfileContext) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let profile = home.join("profiles/test");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("package.json"),
        json!({ "name": "dsh-profile-test", "dsh": { "profile": { "bundles": ["fake-base"] } } })
            .to_string(),
    )
    .unwrap();
    // A bundle below the user layer, found through the profile's node_modules.
    let bundle = profile.join("node_modules/fake-base");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(
        bundle.join("package.json"),
        json!({ "name": "fake-base", "version": "1.0.0", "dsh": { "bundle": { "patch": "base.yml" } } })
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        bundle.join("base.yml"),
        "- insert:\n    - id: from-base\n      name: echo\n",
    )
    .unwrap();
    std::fs::write(profile.join("cordis.patch.yml"), USER).unwrap();
    std::fs::write(
        home.join("cordis.patch.yml"),
        "- id: main\n  disabled: false\n",
    )
    .unwrap();
    let context = ProfileContext {
        name: "test".into(),
        dir: profile,
        install_anchor: dir.path().join("package.json"),
        home,
        overlays: vec![],
        telemetry_disabled: None,
        runtime_version: None,
    };
    (dir, context)
}

async fn start(context: &ProfileContext, log: &Log) -> (Ctx, Loader, Vec<std::path::PathBuf>) {
    let root = Ctx::root().unwrap();
    let loaded = profile::load(context).unwrap();
    root.provide(ProfileContextService::new(context, &loaded))
        .unwrap();
    let plugin = LoaderPlugin::new(builtins(log), profile::loader_options(context));
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    let report = loader
        .reconcile(loaded.layers, Some(loaded.editable))
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    (root, loader, loaded.files)
}

fn state(loader: &Loader, id: &str) -> Option<FiberState> {
    match loader.get(id)?.status {
        EntryStatus::Running(s) => Some(s.state),
        _ => None,
    }
}

fn user_file(context: &ProfileContext) -> String {
    std::fs::read_to_string(context.user_layer_path()).unwrap()
}

#[tokio::test]
async fn expressions_see_the_profile_context() {
    let (_dir, context) = setup();
    let log = Log::default();
    let (_root, loader, _) = start(&context, &log).await;
    assert_eq!(state(&loader, "gated"), Some(FiberState::Active));
    assert_eq!(
        loader.evaluated("gated").unwrap().unwrap(),
        json!({ "dir": context.dir.to_string_lossy() })
    );
}

#[tokio::test]
async fn edits_go_to_the_user_file_and_keep_comments() {
    let (_dir, context) = setup();
    let log = Log::default();
    let (_root, loader, _) = start(&context, &log).await;

    // A row the home layer overrides cannot be changed from the user layer.
    let err = loader.set_disabled("main", true).await.unwrap_err();
    assert!(
        matches!(err, LoaderError::OverriddenByLayer { ref layer, .. } if layer == "home"),
        "{err:?}"
    );

    loader
        .update("from-base", json!({ "level": 3 }))
        .await
        .unwrap();
    loader
        .create(
            NewEntry {
                id: Some("local".into()),
                name: "echo".into(),
                config: json!({ "path": "./x" }),
                ..NewEntry::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
    let text = user_file(&context);
    assert!(
        text.starts_with("# Your patch layer.\n\n# the main row\n- insert:\n"),
        "{text}"
    );
    assert!(
        text.contains("disabled: !!js \"!ctx.get('profileContext')\""),
        "{text}"
    );
    // Comments are kept per patch: the changed patch is rewritten.
    assert!(!text.contains("# keep this comment"), "{text}");
    let reread = profile::load(&context).unwrap();
    let rows = rutis_loader::apply_patches(&reread.layers).flat;
    let row = |id: &str| {
        rows.iter()
            .find(|r| r.id.as_deref() == Some(id))
            .unwrap()
            .value
            .clone()
    };
    assert_eq!(row("from-base")["config"], json!({ "level": 3 }));
    assert_eq!(row("local")["config"], json!({ "path": "./x" }));
}

#[tokio::test]
async fn two_writers_keep_both_edits() {
    let (_dir, context) = setup();
    let (log_a, log_b) = (Log::default(), Log::default());
    let (_ra, a, _) = start(&context, &log_a).await;
    let (_rb, b, _) = start(&context, &log_b).await;
    a.update("from-base", json!({ "level": 10 })).await.unwrap();
    // b read the file before a wrote: its save conflicts and replays.
    b.create(
        NewEntry {
            id: Some("b-row".into()),
            name: "echo".into(),
            ..NewEntry::default()
        },
        None,
        None,
    )
    .await
    .unwrap();
    let reread = profile::load(&context).unwrap();
    let rows = rutis_loader::apply_patches(&reread.layers).flat;
    assert!(rows.iter().any(|r| r.id.as_deref() == Some("b-row")));
    let from_base = rows
        .iter()
        .find(|r| r.id.as_deref() == Some("from-base"))
        .unwrap();
    assert_eq!(from_base.value["config"], json!({ "level": 10 }));
    assert!(!Path::new(&format!(
        "{}.lock",
        context.dir.join("package.json").display()
    ))
    .exists());
}

#[tokio::test]
async fn external_changes_reload() {
    let (_dir, context) = setup();
    let log = Log::default();
    let (_root, loader, files) = start(&context, &log).await;
    let reloads = Arc::new(Mutex::new(Vec::new()));
    let sink = reloads.clone();
    let _watcher = watch::watch(
        loader.clone(),
        context.clone(),
        files,
        Duration::from_millis(20),
        move |reload| sink.lock().unwrap().push(format!("{reload:?}")),
    );
    let mut text = user_file(&context);
    text.push_str("- insert:\n    - id: added\n      name: echo\n");
    std::fs::write(context.user_layer_path(), &text).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state(&loader, "added") != Some(FiberState::Active) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("reload: {:?}", reloads.lock().unwrap()));

    // A broken file keeps the running tree.
    std::fs::write(context.user_layer_path(), "- [ unclosed\n").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !reloads
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.starts_with("Failed"))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("failed reload reported");
    assert_eq!(state(&loader, "added"), Some(FiberState::Active));
}
