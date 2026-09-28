//! Executes generated dispatch against an ordinary, local rutis context.

use std::future::Future;
use std::sync::Arc;

use rutis::{BoxFuture, Ctx};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::task::JoinSet;

use crate::Error;

pub trait Dispatch: Send + Sync + 'static {
    fn call<'a>(
        &'a self,
        target: &'a str,
        method: &'a str,
        args: Value,
    ) -> BoxFuture<'a, Result<Value, Error>>;
}

pub fn native_error(error: impl std::fmt::Display) -> Error {
    Error::Remote {
        name: "RustError".into(),
        message: error.to_string(),
    }
}

fn transport(error: impl std::fmt::Display) -> Error {
    Error::Transport(error.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: u64,
    target: String,
    method: String,
    args: Value,
}

async fn reply(
    writer: &mut OwnedWriteHalf,
    id: u64,
    result: Result<Value, Error>,
) -> Result<(), Error> {
    let mut frame = match result {
        Ok(value) => json!({"id": id, "status": "ok", "value": value}),
        Err(Error::Remote { name, message }) => json!({"id": id, "status": "error", "name": name, "message": message}),
        Err(error) => json!({"id": id, "status": "error", "name": "BindingError", "message": error.to_string()}),
    }.to_string();
    frame.push('\n');
    writer.write_all(frame.as_bytes()).await.map_err(transport)
}

pub async fn serve<F, Fut, D>(mount: F) -> Result<(), Error>
where
    F: FnOnce(Ctx, Value) -> Fut,
    Fut: Future<Output = Result<D, Error>>,
    D: Dispatch,
{
    let socket = std::env::args_os()
        .nth(1)
        .ok_or_else(|| Error::Transport("missing IPC socket".into()))?;
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(transport)?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let ctx = Ctx::root().map_err(native_error)?;
    let mut factory = Some(mount);
    let mut exports: Option<Arc<D>> = None;
    let mut calls = JoinSet::new();
    let result = async {
        loop {
            tokio::select! {
                completed = calls.join_next(), if !calls.is_empty() => {
                    let (id, result) = completed.unwrap().map_err(transport)?;
                    reply(&mut writer, id, result).await?;
                }
                line = lines.next_line() => {
                    let Some(line) = line.map_err(transport)? else { return Ok(()); };
                    let request: Request = serde_json::from_str(&line).map_err(transport)?;
                    if request.target.is_empty() {
                        match request.method.as_str() {
                            "mount" => {
                                let mounted = match factory.take() {
                                    Some(factory) => factory(ctx.clone(), request.args["config"].clone()).await.map(|value| {
                                        exports = Some(Arc::new(value));
                                        Value::Null
                                    }),
                                    None => Err(Error::Value("already mounted".into())),
                                };
                                reply(&mut writer, request.id, mounted).await?;
                            }
                            "dispose" => {
                                // Native consumers finish while the original services remain alive.
                                while let Some(completed) = calls.join_next().await {
                                    let (id, result) = completed.map_err(transport)?;
                                    reply(&mut writer, id, result).await?;
                                }
                                let disposed = ctx.shutdown().await.map(|()| Value::Null).map_err(native_error);
                                reply(&mut writer, request.id, disposed).await?;
                                return Ok(());
                            }
                            _ => reply(&mut writer, request.id, Err(Error::Value("unknown control method".into()))).await?,
                        }
                    } else if let Some(exports) = exports.clone() {
                        calls.spawn(async move {
                            (request.id, exports.call(&request.target, &request.method, request.args).await)
                        });
                    } else {
                        reply(&mut writer, request.id, Err(Error::Value("plugin is not mounted".into()))).await?;
                    }
                }
            }
        }
    }.await;
    calls.shutdown().await;
    let cleanup = ctx.shutdown().await.map_err(native_error);
    writer.shutdown().await.map_err(transport)?;
    result.and(cleanup)
}
