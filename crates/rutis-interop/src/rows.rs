//! Services that rows (plugins loaded one by one into a runtime, see
//! [`Process::load_row_exporting`]) provide to rutis.
//!
//! A row's service is published under [`host_key`]`(name)` as a
//! `dyn HostDispatch`, the same key a Rust-provided host service uses, so a
//! consumer finds it by name whichever side provides it.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::rpc::{Reply, Value as RpcValue};
use crate::{host_key, HostDispatch, Process, Projection};

/// One object a row's service slot holds, as a host service: calls go to
/// that object in the runtime. Async methods return a future.
pub struct RowService {
    process: Arc<Process>,
    handle: String,
    methods: Value,
}

impl RowService {
    pub fn new(process: Arc<Process>, handle: String, methods: Value) -> Self {
        Self {
            process,
            handle,
            methods,
        }
    }
}

impl HostDispatch for RowService {
    fn invoke(&self, method: &str, args: RpcValue) -> Reply {
        match self.methods.get(method).and_then(Value::as_str) {
            Some("async") => {
                let connection = self.process.connection().clone();
                let (handle, method) = (self.handle.clone(), method.to_owned());
                Ok(RpcValue::future(async move {
                    crate::rpc::settle(connection.invoke_async(&handle, &method, args).await?).await
                }))
            }
            Some(_) => self.process.connection().invoke(&self.handle, method, args),
            None => Err(crate::Error::Value(format!(
                "unknown service method {}.{method}",
                self.handle
            ))),
        }
    }

    fn methods(&self) -> Option<Value> {
        Some(self.methods.clone())
    }

    fn origin(&self) -> Option<&Process> {
        Some(&self.process)
    }
}

impl Drop for RowService {
    fn drop(&mut self) {
        self.process.release(&self.handle);
    }
}

/// A projection publishing each service in `provides` (`{ name: { method:
/// "sync" | "async" } }`) as a [`RowService`] under [`host_key`]`(name)`.
/// Attach it from the row's own context and pass it to
/// [`Process::load_row_exporting`] as the observer.
pub fn row_projection(provides: &Map<String, Value>) -> Arc<Projection> {
    let projection = Projection::new();
    for (name, methods) in provides {
        let methods = methods.clone();
        projection.service_keyed::<dyn HostDispatch>(
            name,
            host_key(name),
            move |process, handle| Arc::new(RowService::new(process, handle, methods.clone())),
        );
    }
    projection
}
