//! dsh-llm mounted in a rutis application. Model calls that dsh plugins make
//! through `ctx.llm.stream` reach aimux-llm in this process: the adapter in
//! `cordis/aimux-adapter.ts` pulls neutral parts from [`AimuxBridge`], which
//! the application provides as `ctx.aimux`.
#![cfg(all(unix, dsh_llm))]

rutis_interop::include_mounts!();

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aimux_core::stream_part::StreamPart;
use aimux_core::types::FinishReasonUnified;
use aimux_llm::service::{MessageSpec, PartStream, PromptSpec, ToolCallSpec, ToolSpec};
use aimux_llm::{LlmService, StreamRequest};
use futures::{FutureExt, StreamExt};
use rutis::{BoxFuture, CordisError, Ctx, Disposer};
use rutis_interop::Error;
use tokio_util::sync::CancellationToken;

use dsh::{AimuxPart, AimuxPartKind, AimuxPartReason, AimuxRequest};

/// At most this many parts are returned by one `next`.
const BATCH: usize = 256;

/// Registers `service` as the `aimux` service that the mounted adapter uses.
pub fn provide(ctx: &Ctx, service: Arc<dyn LlmService>) -> Result<Disposer, CordisError> {
    dsh::provide_aimux(ctx, AimuxBridge::new(service))
}

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

impl dsh::AimuxHost for AimuxBridge {
    fn open(&self, request: AimuxRequest) -> BoxFuture<'static, Result<String, Error>> {
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

    fn next(&self, stream: String) -> BoxFuture<'static, Result<Vec<AimuxPart>, Error>> {
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

    fn close(&self, stream: String) -> Result<Option<()>, Error> {
        if let Some(call) = self.calls.lock().unwrap().remove(&stream) {
            call.closed.cancel();
        }
        Ok(None)
    }

    fn list_models(
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

fn stream_request(request: AimuxRequest) -> StreamRequest {
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

fn part(kind: AimuxPartKind) -> AimuxPart {
    AimuxPart {
        kind,
        delta: None,
        id: None,
        name: None,
        arguments: None,
        reason: None,
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        code: None,
        message: None,
    }
}

fn failure(code: &str, message: &str) -> AimuxPart {
    AimuxPart {
        code: Some(code.into()),
        message: Some(message.into()),
        ..part(AimuxPartKind::Error)
    }
}

/// The neutral form of one aimux part; parts the adapter does not use are dropped.
fn neutral(stream_part: StreamPart) -> Option<AimuxPart> {
    let count = |value: Option<u32>| value.map(f64::from);
    Some(match stream_part {
        StreamPart::TextDelta { delta, .. } => AimuxPart {
            delta: Some(delta),
            ..part(AimuxPartKind::Text)
        },
        StreamPart::ReasoningDelta { delta, .. } => AimuxPart {
            delta: Some(delta),
            ..part(AimuxPartKind::Reasoning)
        },
        StreamPart::ToolCall {
            tool_call_id,
            tool_name,
            input,
            ..
        } => AimuxPart {
            id: Some(tool_call_id),
            name: Some(tool_name),
            arguments: Some(match input {
                serde_json::Value::String(text) => text,
                input => input.to_string(),
            }),
            ..part(AimuxPartKind::ToolCall)
        },
        StreamPart::Finish {
            finish_reason,
            usage,
            ..
        } => AimuxPart {
            reason: Some(match finish_reason.unified {
                FinishReasonUnified::Stop => AimuxPartReason::Stop,
                FinishReasonUnified::ToolCalls => AimuxPartReason::ToolCalls,
                FinishReasonUnified::Length => AimuxPartReason::Length,
                _ => AimuxPartReason::Other,
            }),
            input_tokens: count(usage.input_tokens.no_cache.or(usage.input_tokens.total)),
            output_tokens: count(usage.output_tokens.total),
            cache_read_tokens: count(usage.input_tokens.cache_read),
            cache_write_tokens: count(usage.input_tokens.cache_write),
            ..part(AimuxPartKind::Finish)
        },
        StreamPart::Error { error } => failure("PROVIDER", &error.to_string()),
        _ => return None,
    })
}
