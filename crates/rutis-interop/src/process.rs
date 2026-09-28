use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Stdio;
use std::sync::{mpsc, Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::Error;

type Reply = Result<Value, Error>;

enum Pending {
    Sync(mpsc::Sender<Reply>),
    Async(oneshot::Sender<Reply>),
}

impl Pending {
    fn finish(self, reply: Reply) {
        match self {
            Self::Sync(sender) => {
                let _ = sender.send(reply);
            }
            Self::Async(sender) => {
                let _ = sender.send(reply);
            }
        }
    }
}

#[derive(Default)]
struct Calls {
    next: u64,
    pending: HashMap<u64, Pending>,
    closed: Option<Error>,
}

struct Peer {
    writer: Mutex<UnixStream>,
    calls: Mutex<Calls>,
}

#[derive(Deserialize)]
struct Response {
    id: u64,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase", deny_unknown_fields)]
enum Outcome {
    Ok { value: Value },
    Error { name: String, message: String },
}

impl Peer {
    fn close(&self, error: Error) {
        let pending = {
            let mut calls = self.calls.lock().unwrap();
            if calls.closed.is_some() {
                return;
            }
            calls.closed = Some(error.clone());
            std::mem::take(&mut calls.pending)
        };
        let _ = self.writer.lock().unwrap().shutdown(Shutdown::Both);
        for call in pending.into_values() {
            call.finish(Err(error.clone()));
        }
    }

    fn send(&self, target: &str, method: &str, args: Value, pending: Pending) -> Result<(), Error> {
        let id = {
            let mut calls = self.calls.lock().unwrap();
            if let Some(error) = &calls.closed {
                return Err(error.clone());
            }
            calls.next = calls
                .next
                .checked_add(1)
                .ok_or_else(|| Error::Transport("call identifiers exhausted".into()))?;
            let id = calls.next;
            calls.pending.insert(id, pending);
            id
        };
        let mut frame =
            json!({ "id": id, "target": target, "method": method, "args": args }).to_string();
        frame.push('\n');
        let result = self.writer.lock().unwrap().write_all(frame.as_bytes());
        if let Err(error) = result {
            let error = Error::Transport(error.to_string());
            self.close(error.clone());
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.writer.get_mut().unwrap().shutdown(Shutdown::Both);
    }
}

/// Owns one native Cordis process and its generated service bindings.
pub struct Process {
    peer: Arc<Peer>,
    child: tokio::sync::Mutex<tokio::process::Child>,
    _directory: tempfile::TempDir,
}

impl Process {
    pub async fn launch(
        node_package: &Path,
        plugin: &Path,
        config: Value,
        services: Value,
    ) -> Result<Arc<Self>, Error> {
        let directory = tempfile::Builder::new()
            .prefix("rutis-mount-")
            .tempdir()
            .map_err(|error| Error::Transport(error.to_string()))?;
        let socket = directory.path().join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket)
            .map_err(|error| Error::Transport(error.to_string()))?;
        let mut child = tokio::process::Command::new("node")
            .arg("--import")
            .arg("tsx")
            .arg(node_package.join("src/runner.mjs"))
            .arg(&socket)
            .arg(plugin)
            .current_dir(node_package)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| Error::Transport(error.to_string()))?;
        let stream = tokio::select! {
            accepted = listener.accept() => accepted
                .map_err(|error| Error::Transport(error.to_string()))?.0,
            status = child.wait() => return Err(Error::Transport(format!("Cordis process exited before connecting: {status:?}"))),
        };
        let stream = stream
            .into_std()
            .map_err(|error| Error::Transport(error.to_string()))?;
        stream
            .set_nonblocking(false)
            .map_err(|error| Error::Transport(error.to_string()))?;
        let reader = stream
            .try_clone()
            .map_err(|error| Error::Transport(error.to_string()))?;
        let peer = Arc::new(Peer {
            writer: Mutex::new(stream),
            calls: Mutex::new(Calls::default()),
        });
        let weak = Arc::downgrade(&peer);
        std::thread::Builder::new()
            .name("rutis-mount-reader".into())
            .spawn(move || {
                let result: Result<(), Error> = (|| {
                    for line in BufReader::new(reader).lines() {
                        let line = line.map_err(|error| Error::Transport(error.to_string()))?;
                        let response: Response = serde_json::from_str(&line).map_err(|error| {
                            Error::Transport(format!("invalid response: {error}"))
                        })?;
                        let Some(peer) = weak.upgrade() else {
                            return Ok(());
                        };
                        let pending = peer.calls.lock().unwrap().pending.remove(&response.id);
                        let Some(pending) = pending else {
                            return Err(Error::Transport("response for unknown call".into()));
                        };
                        pending.finish(match response.outcome {
                            Outcome::Ok { value } => Ok(value),
                            Outcome::Error { name, message } => {
                                Err(Error::Remote { name, message })
                            }
                        });
                    }
                    Err(Error::Transport("Cordis process disconnected".into()))
                })();
                if let (Err(error), Some(peer)) = (result, weak.upgrade()) {
                    peer.close(error);
                }
            })
            .map_err(|error| Error::Transport(error.to_string()))?;
        let process = Arc::new(Self {
            peer,
            child: tokio::sync::Mutex::new(child),
            _directory: directory,
        });
        process
            .call_async(
                "",
                "mount",
                json!({ "config": config, "services": services }),
            )
            .await?;
        Ok(process)
    }

    pub fn call(&self, service: &str, method: &str, args: Value) -> Reply {
        let (sender, receiver) = mpsc::channel();
        self.peer
            .send(service, method, args, Pending::Sync(sender))?;
        receiver
            .recv()
            .map_err(|error| Error::Transport(error.to_string()))?
    }

    pub async fn call_async(&self, service: &str, method: &str, args: Value) -> Reply {
        let (sender, receiver) = oneshot::channel();
        self.peer
            .send(service, method, args, Pending::Async(sender))?;
        receiver
            .await
            .map_err(|error| Error::Transport(error.to_string()))?
    }

    pub async fn dispose(&self) -> Result<(), Error> {
        let result = self.call_async("", "dispose", Value::Null).await;
        self.peer
            .close(Error::Transport("plugin has been disposed".into()));
        let status = self
            .child
            .lock()
            .await
            .wait()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;
        result?;
        if !status.success() {
            return Err(Error::Transport(format!("Cordis process exited: {status}")));
        }
        Ok(())
    }
}
