//! The `aimux` service the mounted dsh adapter (`dsh/aimux/src/adapter.ts`)
//! uses: each model call is an aimux-llm stream the adapter reads in batches.
//! The request and part shapes mirror the adapter's TypeScript declarations;
//! each mount implements its generated `AimuxHost` trait through
//! [`serve_aimux!`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aimux_core::stream_part::StreamPart;
use aimux_core::types::FinishReasonUnified;
use aimux_llm::service::{MessageSpec, PartStream, PromptSpec, ToolCallSpec, ToolSpec};
use aimux_llm::{LlmService, StreamRequest};
use futures::{FutureExt, StreamExt};
use rutis::BoxFuture;
use rutis_interop::Error;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// One model call (`AimuxRequest` in the adapter).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Request {
    provider: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    system: Option<String>,
    messages: Vec<Message>,
    tools: Vec<Tool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    role: String,
    text: String,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct Tool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<serde_json::Value>,
}

/// One neutral stream part (`AimuxPart` in the adapter).
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Part {
    kind: PartKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<PartReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
enum PartKind {
    #[default]
    Text,
    Reasoning,
    ToolCall,
    Finish,
    Error,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
enum PartReason {
    Stop,
    ToolCalls,
    Length,
    Other,
}

/// Converts between this module's shapes and a mount's generated ones, which
/// share the JSON form.
pub(crate) fn convert<T: Serialize, U: serde::de::DeserializeOwned>(value: &T) -> Result<U, Error> {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .map_err(|error| Error::Value(error.to_string()))
}

/// Implements a mount's generated `AimuxHost` trait for [`AimuxBridge`].
macro_rules! serve_aimux {
    ($mount:ident) => {
        impl $mount::AimuxHost for $crate::aimux::AimuxBridge {
            fn open(
                &self,
                request: $mount::AimuxRequest,
            ) -> ::rutis::BoxFuture<'static, Result<String, ::rutis_interop::Error>> {
                match $crate::aimux::convert(&request) {
                    Ok(request) => $crate::aimux::AimuxBridge::open(self, request),
                    Err(error) => Box::pin(async move { Err(error) }),
                }
            }

            fn next(
                &self,
                stream: String,
            ) -> ::rutis::BoxFuture<'static, Result<Vec<$mount::AimuxPart>, ::rutis_interop::Error>>
            {
                let parts = $crate::aimux::AimuxBridge::next(self, stream);
                Box::pin(async move { $crate::aimux::convert(&parts.await?) })
            }

            fn close(&self, stream: String) -> Result<Option<()>, ::rutis_interop::Error> {
                $crate::aimux::AimuxBridge::close(self, &stream);
                Ok(None)
            }

            fn list_models(
                &self,
                provider: String,
                api_key: Option<String>,
            ) -> ::rutis::BoxFuture<'static, Result<Vec<String>, ::rutis_interop::Error>> {
                $crate::aimux::AimuxBridge::list_models(self, provider, api_key)
            }
        }
    };
}
pub(crate) use serve_aimux;

/// At most this many parts are returned by one `next`.
const BATCH: usize = 256;

/// Serves `ctx.aimux` from an aimux-llm service: each call is a stream the
/// adapter reads in batches.
pub struct AimuxBridge {
    service: Arc<dyn LlmService>,
    calls: Mutex<HashMap<String, Arc<Call>>>,
    ids: AtomicU64,
}

struct Call {
    parts: tokio::sync::Mutex<Option<PartStream>>,
    closed: CancellationToken,
}

impl AimuxBridge {
    pub fn new(service: Arc<dyn LlmService>) -> Self {
        Self {
            service,
            calls: Mutex::default(),
            ids: AtomicU64::new(0),
        }
    }

    fn call(&self, id: &str) -> Option<Arc<Call>> {
        self.calls.lock().unwrap().get(id).cloned()
    }
}

impl Drop for AimuxBridge {
    fn drop(&mut self) {
        for call in self.calls.lock().unwrap().values() {
            call.closed.cancel();
        }
    }
}

impl AimuxBridge {
    pub(crate) fn open(&self, request: Request) -> BoxFuture<'static, Result<String, Error>> {
        let id = format!("aimux:{}", self.ids.fetch_add(1, Ordering::Relaxed));
        let call = Arc::new(Call {
            parts: tokio::sync::Mutex::new(None),
            closed: CancellationToken::new(),
        });
        self.calls.lock().unwrap().insert(id.clone(), call.clone());
        let service = self.service.clone();
        Box::pin(async move {
            let opened = tokio::select! {
                opened = service.stream(stream_request(request)) => opened,
                () = call.closed.cancelled() => return Ok(id),
            };
            // A failed start is reported by the first `next`.
            let parts: PartStream = match opened {
                Ok(parts) => parts,
                Err(error) => Box::pin(futures::stream::once(async move { Err(error) })),
            };
            *call.parts.lock().await = Some(parts);
            Ok(id)
        })
    }

    pub(crate) fn next(&self, stream: String) -> BoxFuture<'static, Result<Vec<Part>, Error>> {
        let call = self.call(&stream);
        Box::pin(async move {
            let Some(call) = call else {
                return Ok(Vec::new());
            };
            let mut guard = call.parts.lock().await;
            let mut batch = Vec::new();
            let mut ended = false;
            while batch.is_empty() && !ended {
                let Some(parts) = guard.as_mut() else { break };
                let mut item = tokio::select! {
                    item = parts.next() => item,
                    () = call.closed.cancelled() => None,
                };
                ended = item.is_none();
                // Take whatever else is already produced without waiting.
                while let Some(result) = item.take() {
                    match result {
                        Ok(part) => batch.extend(neutral(part)),
                        Err(error) => {
                            batch.push(failure(&error.code, &error.message));
                            ended = true;
                            break;
                        }
                    }
                    if batch.len() >= BATCH {
                        break;
                    }
                    item = match parts.next().now_or_never() {
                        Some(Some(next)) => Some(next),
                        Some(None) => {
                            ended = true;
                            None
                        }
                        None => None,
                    };
                }
            }
            if ended {
                *guard = None;
            }
            Ok(batch)
        })
    }

    pub(crate) fn close(&self, stream: &str) {
        if let Some(call) = self.calls.lock().unwrap().remove(stream) {
            call.closed.cancel();
        }
    }

    pub(crate) fn list_models(
        &self,
        provider: String,
        api_key: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<String>, Error>> {
        let service = self.service.clone();
        Box::pin(async move {
            let models = service
                .list_models(&provider, api_key.as_deref())
                .await
                .map_err(|error| Error::Value(error.to_string()))?;
            Ok(models.into_iter().map(|model| model.id).collect())
        })
    }
}

fn stream_request(request: Request) -> StreamRequest {
    StreamRequest {
        provider: Some(request.provider),
        model: Some(request.model),
        api_key: request.api_key,
        options: PromptSpec {
            system: request.system,
            messages: request
                .messages
                .into_iter()
                .map(|message| MessageSpec {
                    role: Some(message.role),
                    text: message.text,
                    tool_calls: message
                        .tool_calls
                        .unwrap_or_default()
                        .into_iter()
                        .map(|call| ToolCallSpec {
                            id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                        })
                        .collect(),
                    tool_call_id: message.tool_call_id,
                    tool_name: message.tool_name,
                    is_error: message.is_error,
                })
                .collect(),
            tools: request
                .tools
                .into_iter()
                .map(|tool| ToolSpec {
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.parameters,
                })
                .collect(),
        },
    }
}

fn part(kind: PartKind) -> Part {
    Part {
        kind,
        ..Part::default()
    }
}

fn failure(code: &str, message: &str) -> Part {
    Part {
        code: Some(code.into()),
        message: Some(message.into()),
        ..part(PartKind::Error)
    }
}

/// The neutral form of one aimux part; parts the adapter does not use are dropped.
fn neutral(stream_part: StreamPart) -> Option<Part> {
    let count = |value: Option<u32>| value.map(f64::from);
    Some(match stream_part {
        StreamPart::TextDelta { delta, .. } => Part {
            delta: Some(delta),
            ..part(PartKind::Text)
        },
        StreamPart::ReasoningDelta { delta, .. } => Part {
            delta: Some(delta),
            ..part(PartKind::Reasoning)
        },
        StreamPart::ToolCall {
            tool_call_id,
            tool_name,
            input,
            ..
        } => Part {
            id: Some(tool_call_id),
            name: Some(tool_name),
            arguments: Some(match input {
                serde_json::Value::String(text) => text,
                input => input.to_string(),
            }),
            ..part(PartKind::ToolCall)
        },
        StreamPart::Finish {
            finish_reason,
            usage,
            ..
        } => Part {
            reason: Some(match finish_reason.unified {
                FinishReasonUnified::Stop => PartReason::Stop,
                FinishReasonUnified::ToolCalls => PartReason::ToolCalls,
                FinishReasonUnified::Length => PartReason::Length,
                _ => PartReason::Other,
            }),
            input_tokens: count(usage.input_tokens.no_cache.or(usage.input_tokens.total)),
            output_tokens: count(usage.output_tokens.total),
            cache_read_tokens: count(usage.input_tokens.cache_read),
            cache_write_tokens: count(usage.input_tokens.cache_write),
            ..part(PartKind::Finish)
        },
        StreamPart::Error { error } => failure("PROVIDER", &error.to_string()),
        _ => return None,
    })
}
