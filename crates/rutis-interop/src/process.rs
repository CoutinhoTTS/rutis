use crate::rpc::{Connection, Dispatch, Reply, Value as RpcValue};
use crate::Error;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

struct NoExports;
impl Dispatch for NoExports {
    fn invoke(&self, _: &Connection, _: &str, _: &str, _: RpcValue) -> Reply {
        Err(Error::Value(
            "application has no exported service target".into(),
        ))
    }
}

/// Owns one native Cordis process and its generated service bindings.
pub struct Process {
    peer: Connection,
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
        let peer = Connection::connect(stream, Arc::new(NoExports))?;
        peer.ready().await?;
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

    pub fn connection(&self) -> &Connection {
        &self.peer
    }

    pub fn call(&self, service: &str, method: &str, args: Value) -> Result<Value, Error> {
        self.peer.invoke(service, method, args.into())?.json()
    }

    pub async fn call_async(
        &self,
        service: &str,
        method: &str,
        args: Value,
    ) -> Result<Value, Error> {
        let value = self.peer.invoke_async(service, method, args.into()).await?;
        match value {
            RpcValue::Reference(reference) if reference.is_future() => {
                reference.wait_async().await?.json()
            }
            value => value.json(),
        }
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

impl Drop for Process {
    fn drop(&mut self) {
        self.peer.close(Error::Transport("process dropped".into()));
    }
}
