//! Runs generated dispatch against an ordinary local rutis context.
use crate::cordis::rpc::{self, Connection, Reply, Value as RpcValue};
use crate::cordis::Error;
use rutis::Ctx;
use serde_json::Value;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub trait Dispatch: Send + Sync + 'static {
    fn invoke(&self, target: &str, method: &str, args: RpcValue) -> Reply;
}

pub use crate::cordis::native_error;
fn transport(error: impl std::fmt::Display) -> Error {
    Error::Transport(error.to_string())
}

struct Root<F, D> {
    ctx: Ctx,
    factory: Mutex<Option<F>>,
    exports: Arc<Mutex<Option<Arc<D>>>>,
    closing: AtomicBool,
}
impl<F, Fut, D> rpc::Dispatch for Root<F, D>
where
    F: FnOnce(Ctx, Value) -> Fut + Send + 'static,
    Fut: Future<Output = Result<D, Error>> + Send + 'static,
    D: Dispatch,
{
    fn invoke(&self, peer: &Connection, target: &str, method: &str, args: RpcValue) -> Reply {
        if self.closing.load(Ordering::SeqCst) {
            return Err(Error::Value("plugin is closing".into()));
        }
        if target.is_empty() {
            match method {
                "mount" => {
                    let factory = self
                        .factory
                        .lock()
                        .unwrap()
                        .take()
                        .ok_or_else(|| Error::Value("already mounted".into()))?;
                    let ctx = self.ctx.clone();
                    let exports = self.exports.clone();
                    let config = args.json()?["config"].clone();
                    Ok(RpcValue::control_future(async move {
                        let value = factory(ctx, config).await?;
                        *exports.lock().unwrap() = Some(Arc::new(value));
                        Ok(Value::Null.into())
                    }))
                }
                "dispose" => {
                    self.closing.store(true, Ordering::SeqCst);
                    let cleanup = self.ctx.shutdown();
                    let peer = peer.clone();
                    Ok(RpcValue::control_future(async move {
                        let result = cleanup.await.map_err(native_error);
                        peer.drain().await;
                        result?;
                        Ok(Value::Null.into())
                    }))
                }
                _ => Err(Error::Value("unknown control method".into())),
            }
        } else {
            let exports = self
                .exports
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| Error::Value("plugin is not mounted".into()))?;
            exports.invoke(target, method, args)
        }
    }
}

pub async fn serve<F, Fut, D>(mount: F) -> Result<(), Error>
where
    F: FnOnce(Ctx, Value) -> Fut + Send + 'static,
    Fut: Future<Output = Result<D, Error>> + Send + 'static,
    D: Dispatch,
{
    let socket = std::env::args_os()
        .nth(1)
        .ok_or_else(|| transport("missing IPC socket"))?;
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(transport)?
        .into_std()
        .map_err(transport)?;
    let ctx = Ctx::root().map_err(native_error)?;
    let peer = Connection::connect(
        stream,
        Arc::new(Root {
            ctx: ctx.clone(),
            factory: Mutex::new(Some(mount)),
            exports: Arc::new(Mutex::new(None)),
            closing: AtomicBool::new(false),
        }),
    )?;
    peer.ready().await?;
    peer.closed().await;
    ctx.shutdown().await.map_err(native_error)
}
