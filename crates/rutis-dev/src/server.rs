use std::path::{Path, PathBuf};
use std::sync::Arc;

use rutis::{
    BoxFuture, CordisError, Ctx, Event, EventKey, FiberStatusChanged, Listener, ServiceChanged,
};
use rutis_loader::{Loader, LoaderChanged, Patch, PendingEditDropped};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::describe;

pub const PROTOCOL_VERSION: u32 = 1;

/// A mutating command and its outcome.
#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub command: String,
    pub request: Value,
    pub result: Result<Value, String>,
}

pub struct DevOptions {
    pub socket: PathBuf,
    /// Host identity returned by `hello` (SDK id, toolchain, version, ...).
    pub hello: Value,
    /// The overlay layer loaded rows go to.
    pub overlay: String,
    /// Called for every `load`, `swap` and `unload-dev`; stderr by default.
    pub audit: Arc<dyn Fn(&AuditRecord) + Send + Sync>,
}

impl DevOptions {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            hello: Value::Null,
            overlay: "dev".into(),
            audit: Arc::new(|record: &AuditRecord| {
                eprintln!(
                    "[rutis-dev] {} {} -> {}",
                    record.command,
                    record.request,
                    match &record.result {
                        Ok(_) => "ok".to_owned(),
                        Err(e) => format!("error: {e}"),
                    }
                )
            }),
        }
    }
}

struct Shared {
    root: Ctx,
    loader: Loader,
    hello: Value,
    overlay: String,
    audit: Arc<dyn Fn(&AuditRecord) + Send + Sync>,
    /// Rows the channel loaded, in load order.
    rows: tokio::sync::Mutex<Vec<Value>>,
}

/// A running channel. Dropping it stops listening and removes the socket;
/// rows it loaded stay until unloaded or the overlay is cleared.
pub struct DevChannel {
    task: tokio::task::JoinHandle<()>,
    socket: PathBuf,
}

impl Drop for DevChannel {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl DevChannel {
    /// Listen on `options.socket`. A stale socket (nobody listening) is
    /// replaced; a live one is an error, and so is anything at that path
    /// that is not a socket — a mistyped path must not delete a file.
    pub async fn start(root: Ctx, loader: Loader, options: DevOptions) -> std::io::Result<Self> {
        use std::os::unix::fs::FileTypeExt;

        let path = options.socket.clone();
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
            Ok(meta) if !meta.file_type().is_socket() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            Ok(_) => {
                if UnixStream::connect(&path).await.is_ok() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("a dev channel already listens on {}", path.display()),
                    ));
                }
                std::fs::remove_file(&path)?;
            }
        }
        let listener = UnixListener::bind(&path)?;
        restrict(&path)?;
        let shared = Arc::new(Shared {
            root,
            loader,
            hello: options.hello,
            overlay: options.overlay,
            audit: options.audit,
            rows: tokio::sync::Mutex::new(Vec::new()),
        });
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(shared, stream).await;
                });
            }
        });
        Ok(Self { task, socket: path })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

async fn write(stream: &mut (impl AsyncWriteExt + Unpin), value: &Value) -> std::io::Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).await
}

async fn serve(shared: Arc<Shared>, stream: UnixStream) -> std::io::Result<()> {
    let (read, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(Value::Object(map)) => Value::Object(map),
            Ok(_) | Err(_) => {
                write(&mut write_half, &json!({ "req": null, "ok": false, "error": "requests are JSON objects, one per line" })).await?;
                continue;
            }
        };
        // `req` correlates a response with its request; `id` is a row id,
        // an argument of `load`, `swap` and `unload-dev`.
        let req = request.get("req").cloned().unwrap_or(Value::Null);
        let command = request
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if command == "watch" {
            write(
                &mut write_half,
                &json!({ "req": req, "ok": true, "result": { "watching": true } }),
            )
            .await?;
            return watch(&shared, write_half, lines).await;
        }
        let result = handle(&shared, &command, &request).await;
        if matches!(command.as_str(), "load" | "swap" | "unload-dev") {
            (shared.audit)(&AuditRecord {
                command: command.clone(),
                request: request.clone(),
                result: result.clone(),
            });
        }
        let response = match result {
            Ok(result) => json!({ "req": req, "ok": true, "result": result }),
            Err(error) => json!({ "req": req, "ok": false, "error": error }),
        };
        write(&mut write_half, &response).await?;
    }
    Ok(())
}

fn text<'a>(request: &'a Value, key: &str) -> Result<&'a str, String> {
    request
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("`{key}` is required"))
}

fn report(report: &rutis_loader::ReconcileReport) -> Value {
    json!({
        "newFailures": report.new_failures.iter().map(|f| json!({ "id": f.id, "error": f.error })).collect::<Vec<_>>(),
        "failures": report.failures.len(),
        "warnings": report.warnings.iter().map(|w| w.message.clone()).collect::<Vec<_>>(),
    })
}

fn overlay_patches(rows: &[Value]) -> Vec<Patch> {
    rows.iter()
        .map(|row| {
            let parent = row.get("parent").and_then(Value::as_str).map(str::to_owned);
            let mut row = row.clone();
            if let Some(map) = row.as_object_mut() {
                map.remove("parent");
            }
            serde_json::from_value(json!({ "id": parent, "insert": [row] }))
                .expect("an insert patch")
        })
        .collect()
}

async fn handle(shared: &Shared, command: &str, request: &Value) -> Result<Value, String> {
    let loader = &shared.loader;
    let dev_ids = |rows: &[Value]| -> Vec<String> {
        rows.iter()
            .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    };
    match command {
        "hello" => Ok(json!({
            "protocol": PROTOCOL_VERSION,
            "rutisDev": env!("CARGO_PKG_VERSION"),
            "host": shared.hello,
        })),
        "status" => {
            let rows = shared.rows.lock().await;
            let dev = dev_ids(&rows);
            Ok(json!({
                "entries": loader
                    .entries()
                    .iter()
                    .map(|e| describe::entry(e, dev.contains(&e.id)))
                    .collect::<Vec<_>>(),
                "pending": loader.pending().len(),
            }))
        }
        "describe" => {
            let rows = shared.rows.lock().await;
            let dev = dev_ids(&rows);
            let mut out = describe::diagnostics(&shared.root.diagnostics());
            out["entries"] = loader
                .entries()
                .iter()
                .map(|e| {
                    let mut entry = describe::entry(e, dev.contains(&e.id));
                    entry["fiber"] = json!(loader.get(&e.id).and_then(|i| i.plugin).map(|p| p.0));
                    entry
                })
                .collect();
            Ok(out)
        }
        "load" => {
            let name = text(request, "name")?.to_owned();
            let mut rows = shared.rows.lock().await;
            let id = match request.get("id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => id.to_owned(),
                _ => format!("dev-{}", rows.len() + 1),
            };
            if loader.get(&id).is_some() {
                return Err(format!("a row {id:?} already exists"));
            }
            let mut row = Map::new();
            row.insert("id".into(), json!(id));
            row.insert("name".into(), json!(name));
            if let Some(config) = request.get("config") {
                row.insert("config".into(), config.clone());
            }
            if let Some(parent) = request.get("parent").and_then(Value::as_str) {
                row.insert("parent".into(), json!(parent));
            }
            let mut next = rows.clone();
            next.push(Value::Object(row));
            let result = loader
                .set_overlay(&shared.overlay, Some(overlay_patches(&next)))
                .await
                .map_err(|e| e.to_string())?;
            *rows = next;
            Ok(json!({ "id": id, "report": report(&result) }))
        }
        "swap" => {
            let id = text(request, "id")?;
            let result = loader.reload(id).await.map_err(|e| e.to_string())?;
            Ok(json!({ "id": id, "report": report(&result) }))
        }
        "unload-dev" => {
            let id = text(request, "id")?.to_owned();
            let mut rows = shared.rows.lock().await;
            let Some(index) = rows
                .iter()
                .position(|r| r.get("id").and_then(Value::as_str) == Some(id.as_str()))
            else {
                return Err(format!("{id:?} was not loaded through the dev channel"));
            };
            let mut next = rows.clone();
            next.remove(index);
            let patches = (!next.is_empty()).then(|| overlay_patches(&next));
            let result = loader
                .set_overlay(&shared.overlay, patches)
                .await
                .map_err(|e| e.to_string())?;
            *rows = next;
            Ok(json!({ "id": id, "report": report(&result) }))
        }
        "" => Err("`cmd` is required".into()),
        other => Err(format!("unknown command {other:?}")),
    }
}

/// Forwards one event kind to a client as JSON.
struct Forward<E> {
    send: mpsc::UnboundedSender<Value>,
    render: fn(&E) -> Value,
}

impl<E: Event> Listener<E> for Forward<E> {
    fn call<'a>(
        &'a self,
        _ctx: &'a Ctx,
        e: &'a E,
    ) -> BoxFuture<'a, Result<Option<E::Value>, CordisError>> {
        let _ = self.send.send((self.render)(e));
        Box::pin(async { Ok(None) })
    }
}

async fn watch(
    shared: &Shared,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> std::io::Result<()> {
    let (send, mut receive) = mpsc::unbounded_channel();
    let root = &shared.root;
    let events = root.events();
    let subscriptions: Vec<rutis::Disposer> = [
        events.on(
            root,
            &EventKey::<FiberStatusChanged>::of(),
            Forward {
                send: send.clone(),
                render: describe::fiber_status,
            },
        ),
        events.on(
            root,
            &EventKey::<ServiceChanged>::of(),
            Forward {
                send: send.clone(),
                render: describe::service,
            },
        ),
        events.on(
            root,
            &EventKey::<LoaderChanged>::of(),
            Forward {
                send: send.clone(),
                render: describe::loader,
            },
        ),
        events.on(
            root,
            &EventKey::<PendingEditDropped>::of(),
            Forward {
                send,
                render: describe::dropped,
            },
        ),
    ]
    .into_iter()
    .filter_map(Result::ok)
    .collect();
    let outcome = loop {
        tokio::select! {
            event = receive.recv() => match event {
                Some(event) => if let Err(e) = write(&mut write_half, &event).await { break Err(e) },
                None => break Ok(()),
            },
            line = lines.next_line() => match line {
                // Input during a watch is ignored; EOF ends it.
                Ok(Some(_)) => {}
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            },
        }
    };
    for disposer in subscriptions {
        let _ = disposer.dispose().await;
    }
    outcome
}
