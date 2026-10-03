//! The dev channel over a real socket.
#![cfg(unix)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, PluginFactory};
use rutis_dev::{AuditRecord, DevChannel, DevOptions};
use rutis_loader::{Builtins, Layer, LoaderError, LoaderOptions, LoaderPlugin, Resolved, Resolver};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Provides a `u32` (its config's `value`), keyed by its config's `key`
/// when given.
struct Provider(u32, Option<String>);

impl Plugin for Provider {
    fn name(&self) -> &str {
        "provider"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            match &self.1 {
                None => ctx.provide(self.0)?,
                Some(key) => ctx.provide_as::<u32>(
                    rutis::TypeKey::keyed_dynamic::<u32>(key.clone()),
                    Arc::new(self.0),
                )?,
            };
            Ok(Effect::Done)
        })
    }
}

struct ProviderFactory;

impl PluginFactory<Value> for ProviderFactory {
    fn name(&self) -> &str {
        "provider"
    }

    fn build(&self, config: &Value) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(Provider(
            config["value"].as_u64().unwrap_or(0) as u32,
            config["key"].as_str().map(str::to_owned),
        )))
    }
}

/// `provider` resolves to a fresh module each time unless `broken`.
struct Switching {
    broken: Arc<AtomicBool>,
}

impl Resolver for Switching {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        Box::pin(async move {
            if self.broken.load(Ordering::SeqCst) {
                return Err(LoaderError::Resolve {
                    name: name.into(),
                    message: "broken build".into(),
                });
            }
            let mut builtins = Builtins::new();
            builtins.register_raw("provider", ProviderFactory, None);
            builtins.resolve(name).await
        })
    }
}

struct Client {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    async fn connect(path: &std::path::Path) -> Self {
        let (read, write) = UnixStream::connect(path).await.unwrap().into_split();
        Self {
            lines: BufReader::new(read).lines(),
            write,
        }
    }

    async fn send(&mut self, request: Value) -> Value {
        self.write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        self.next().await
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("a line")
            .unwrap()
            .expect("open");
        serde_json::from_str(&line).unwrap()
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    _root: Ctx,
    channel: DevChannel,
    loader: rutis_loader::Loader,
    broken: Arc<AtomicBool>,
    audit: Arc<Mutex<Vec<AuditRecord>>>,
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = Ctx::root().unwrap();
    let broken = Arc::new(AtomicBool::new(false));
    let plugin = LoaderPlugin::new(
        Switching {
            broken: broken.clone(),
        },
        LoaderOptions::default(),
    );
    let loader = plugin.handle();
    root.plugin(plugin).await.unwrap();
    loader
        .reconcile(
            vec![Layer::new(
                "app",
                serde_json::from_value(json!([{ "insert": [{ "id": "base", "name": "provider", "config": { "value": 1 } }] }]))
                    .unwrap(),
            )],
            None,
        )
        .await
        .unwrap();
    let audit = Arc::new(Mutex::new(Vec::new()));
    let sink = audit.clone();
    let mut options = DevOptions::new(dir.path().join("dev.sock"));
    options.hello = json!({ "host": "test" });
    options.audit = Arc::new(move |r: &AuditRecord| sink.lock().unwrap().push(r.clone()));
    let channel = DevChannel::start(root.clone(), loader.clone(), options)
        .await
        .unwrap();
    Setup {
        _dir: dir,
        _root: root,
        channel,
        loader,
        broken,
        audit,
    }
}

#[tokio::test]
async fn hello_status_describe() {
    let s = setup().await;
    let mut c = Client::connect(s.channel.socket()).await;
    let hello = c.send(json!({ "req": 1, "cmd": "hello" })).await;
    assert_eq!(hello["req"], 1);
    assert_eq!(hello["result"]["protocol"], 1);
    assert_eq!(hello["result"]["host"], json!({ "host": "test" }));

    let status = c.send(json!({ "req": 2, "cmd": "status" })).await;
    let entries = status["result"]["entries"].as_array().unwrap();
    assert_eq!(entries[0]["id"], "base");
    assert_eq!(entries[0]["state"], "Active");
    assert_eq!(entries[0]["dev"], false);

    let describe = c.send(json!({ "req": 3, "cmd": "describe" })).await["result"].clone();
    assert!(describe["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "provider"));
    assert!(describe["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["key"] == "u32"));
    assert_eq!(describe["entries"][0]["id"], "base");

    let bad = c.send(json!({ "req": 4, "cmd": "nope" })).await;
    assert_eq!(bad["ok"], false);
    c.write.write_all(b"not json\n").await.unwrap();
    assert_eq!(c.next().await["ok"], false);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(s.channel.socket())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[tokio::test]
async fn load_swap_unload_and_watch() {
    let s = setup().await;
    let mut watcher = Client::connect(s.channel.socket()).await;
    assert_eq!(
        watcher.send(json!({ "req": "w", "cmd": "watch" })).await["ok"],
        true
    );

    let mut c = Client::connect(s.channel.socket()).await;
    let loaded = c
        .send(json!({ "cmd": "load", "id": "mine", "name": "provider", "config": { "value": 7, "key": "mine" } }))
        .await;
    assert_eq!(loaded["ok"], true, "{loaded}");
    let status = c.send(json!({ "cmd": "status" })).await;
    let mine = status["result"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "mine")
        .unwrap()
        .clone();
    assert_eq!(mine["dev"], true);
    assert_eq!(mine["state"], "Active");
    // The user's layers are untouched.
    assert_eq!(s.loader.layers()[0].patches.len(), 1);

    // The watcher saw the row start and its service appear.
    let mut seen = Vec::new();
    while !(seen
        .iter()
        .any(|e: &Value| e["event"] == "service" && e["change"] == "provided")
        && seen.iter().any(|e| e["event"] == "loader"))
    {
        seen.push(watcher.next().await);
    }
    assert!(
        seen.iter()
            .any(|e| e["event"] == "fiber" && e["to"] == "Active"),
        "{seen:?}"
    );

    // A broken build: swap fails and the old version keeps running.
    let plugin = s.loader.get("mine").unwrap().plugin;
    s.broken.store(true, Ordering::SeqCst);
    let failed = c.send(json!({ "cmd": "swap", "id": "mine" })).await;
    assert_eq!(failed["ok"], false);
    assert!(failed["error"].as_str().unwrap().contains("broken build"));
    assert_eq!(s.loader.get("mine").unwrap().plugin, plugin);
    s.broken.store(false, Ordering::SeqCst);
    let swapped = c.send(json!({ "cmd": "swap", "id": "mine" })).await;
    assert_eq!(swapped["ok"], true, "{swapped}");
    assert_eq!(
        s.loader.get("mine").unwrap().plugin,
        plugin,
        "same identity: in place"
    );

    // Only dev rows can be unloaded; a duplicate id is refused.
    assert_eq!(
        c.send(json!({ "cmd": "unload-dev", "id": "base" })).await["ok"],
        false
    );
    assert_eq!(
        c.send(json!({ "cmd": "load", "id": "base", "name": "provider" }))
            .await["ok"],
        false
    );
    assert_eq!(
        c.send(json!({ "cmd": "unload-dev", "id": "mine" })).await["ok"],
        true
    );
    assert!(s.loader.get("mine").is_none());

    let audit = s.audit.lock().unwrap();
    let commands: Vec<&str> = audit.iter().map(|r| r.command.as_str()).collect();
    assert_eq!(
        commands,
        ["load", "swap", "swap", "unload-dev", "load", "unload-dev"]
    );
    assert!(audit[1].result.is_err());
}

#[tokio::test]
async fn a_live_socket_is_not_replaced_but_a_stale_one_is() {
    let s = setup().await;
    let path = s.channel.socket().to_owned();
    let err = DevChannel::start(s._root.clone(), s.loader.clone(), DevOptions::new(&path))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    drop(s.channel);
    assert!(!path.exists(), "dropping the channel removes the socket");
    // A stale file left behind by a crash is replaced.
    std::os::unix::net::UnixListener::bind(&path).unwrap();
    let again = DevChannel::start(s._root.clone(), s.loader.clone(), DevOptions::new(&path))
        .await
        .unwrap();
    assert!(again.socket().exists());
}

#[tokio::test]
async fn a_path_that_is_not_a_socket_is_never_removed() {
    let s = setup().await;
    let dir = s._dir.path();
    let start = |path: std::path::PathBuf| {
        DevChannel::start(s._root.clone(), s.loader.clone(), DevOptions::new(path))
    };

    let file = dir.join("notes.txt");
    std::fs::write(&file, "keep me").unwrap();
    let err = start(file.clone()).await.err().unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me");

    // A symlink is refused as such, whether to a file or a stale socket.
    let link = dir.join("link.sock");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    assert_eq!(
        start(link.clone()).await.err().unwrap().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    let stale = dir.join("stale.sock");
    std::os::unix::net::UnixListener::bind(&stale).unwrap();
    let to_socket = dir.join("to-socket.sock");
    std::os::unix::fs::symlink(&stale, &to_socket).unwrap();
    assert_eq!(
        start(to_socket.clone()).await.err().unwrap().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(std::fs::symlink_metadata(&link).is_ok());
    assert!(std::fs::symlink_metadata(&to_socket).is_ok());
    assert!(stale.exists());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me");
}

/// Run the `rutis-dev` binary against the channel.
async fn cli(socket: &std::path::Path, args: &[&str]) -> (bool, Value) {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_rutis-dev"))
        .arg(socket)
        .args(args)
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value = serde_json::from_str(&stdout).unwrap_or_else(|_| {
        panic!(
            "stdout {stdout:?}, stderr {:?}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.success(), value)
}

#[tokio::test]
async fn the_cli_keeps_the_row_id() {
    let s = setup().await;
    let socket = s.channel.socket();
    let (ok, loaded) = cli(
        socket,
        &[
            "load",
            r#"{"name": "provider", "id": "cli-row", "config": {"value": 3, "key": "cli"}}"#,
        ],
    )
    .await;
    assert!(ok, "{loaded}");
    assert_eq!(loaded["req"], 1);
    assert_eq!(loaded["result"]["id"], "cli-row");
    assert!(s.loader.get("cli-row").is_some());
    assert!(s.loader.get("1").is_none());

    let (ok, swapped) = cli(socket, &["swap", r#"{"id": "cli-row"}"#]).await;
    assert!(ok, "{swapped}");
    assert_eq!(swapped["result"]["id"], "cli-row");
    let (ok, unloaded) = cli(socket, &["unload-dev", r#"{"id": "cli-row"}"#]).await;
    assert!(ok, "{unloaded}");
    assert!(s.loader.get("cli-row").is_none());

    // A failure exits non-zero.
    let (ok, missing) = cli(socket, &["swap", r#"{"id": "nope"}"#]).await;
    assert!(!ok);
    assert_eq!(missing["ok"], false);
}
