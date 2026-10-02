// Shared by several test binaries; each uses part of it.
#![allow(dead_code)]

//! A scripted aimux service and the dsh mount the tests drive.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aimux_core::stream_part::StreamPart;
use aimux_core::types::{FinishReason, FinishReasonUnified, TokenUsage, Usage};
use aimux_llm::service::PartStream;
use aimux_llm::{LlmService, LlmServiceError, ModelBrief, StreamRequest};
use rutis::Ctx;
use rutis_dsh::agent;
use serde_json::json;

/// Plays one scripted response per provider and records the requests.
#[derive(Default)]
pub struct Scripted {
    pub requests: Mutex<Vec<StreamRequest>>,
    pub dropped: Arc<AtomicBool>,
    /// `(provider, key)` of each model listing.
    pub listed: Mutex<Vec<(String, Option<String>)>>,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl LlmService for Scripted {
    async fn stream(&self, req: StreamRequest) -> Result<PartStream, LlmServiceError> {
        let provider = req.provider.clone().unwrap_or_default();
        self.requests.lock().unwrap().push(req);
        match provider.as_str() {
            "scripted" => Ok(Box::pin(async_stream::stream! {
                yield Ok(StreamPart::StreamStart { warnings: Vec::new() });
                for delta in ["hel", "lo"] {
                    yield Ok(StreamPart::TextDelta { id: "t".into(), delta: delta.into(), provider_metadata: None });
                }
                yield Ok(StreamPart::ToolCall {
                    tool_call_id: "call-1".into(),
                    tool_name: "lookup".into(),
                    input: json!({ "q": "m2" }),
                    provider_executed: None,
                    dynamic: None,
                    thought_signature: None,
                    provider_metadata: None,
                });
                yield Ok(StreamPart::Finish {
                    finish_reason: FinishReason { unified: FinishReasonUnified::ToolCalls, raw: None },
                    usage: Usage {
                        input_tokens: TokenUsage { total: Some(12), no_cache: Some(10), cache_read: Some(2), ..Default::default() },
                        output_tokens: TokenUsage { total: Some(5), ..Default::default() },
                        raw: None,
                    },
                    provider_metadata: None,
                });
            })),
            "endless" => {
                let flag = DropFlag(self.dropped.clone());
                Ok(Box::pin(async_stream::stream! {
                    let _flag = flag;
                    loop {
                        yield Ok(StreamPart::TextDelta { id: "t".into(), delta: "x".into(), provider_metadata: None });
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }))
            }
            // An agent turn: call `lookup`, then answer with its result.
            "agent" => {
                let requests = self.requests.lock().unwrap();
                let last = requests
                    .last()
                    .unwrap()
                    .options
                    .messages
                    .last()
                    .unwrap()
                    .clone();
                drop(requests);
                let parts = if last.role.as_deref() == Some("tool") {
                    let answer = format!("The code is {}.", last.text);
                    vec![
                        StreamPart::TextDelta {
                            id: "t".into(),
                            delta: answer,
                            provider_metadata: None,
                        },
                        finish(FinishReasonUnified::Stop),
                    ]
                } else {
                    vec![
                        StreamPart::ToolCall {
                            tool_call_id: "call-1".into(),
                            tool_name: "lookup".into(),
                            input: json!({ "q": "m2" }),
                            provider_executed: None,
                            dynamic: None,
                            thought_signature: None,
                            provider_metadata: None,
                        },
                        finish(FinishReasonUnified::ToolCalls),
                    ]
                };
                Ok(Box::pin(futures::stream::iter(parts.into_iter().map(Ok))))
            }
            _ => Err(LlmServiceError::new("AUTH", "no key for this provider")),
        }
    }

    async fn list_models(
        &self,
        provider: &str,
        key: Option<&str>,
    ) -> Result<Vec<ModelBrief>, LlmServiceError> {
        self.listed
            .lock()
            .unwrap()
            .push((provider.into(), key.map(Into::into)));
        Ok(vec![ModelBrief {
            id: format!("{provider}-chat"),
            ..Default::default()
        }])
    }
}

pub async fn mounted(service: Arc<Scripted>) -> Ctx {
    mount(service, &["scripted", "endless", "failing", "agent"]).await
}

/// Mounts the dsh composition with `service` serving the given provider routes.
pub async fn mount(service: Arc<dyn LlmService>, providers: &[&str]) -> Ctx {
    let route = || agent::Route {
        provider: None,
        api_key_env: None,
        display_name: None,
    };
    mount_routes(
        service,
        providers.iter().map(|provider| (*provider, route())),
    )
    .await
}

/// Mounts the dsh composition with `service` serving the given routes.
pub async fn mount_routes(
    service: Arc<dyn LlmService>,
    routes: impl IntoIterator<Item = (&str, agent::Route)>,
) -> Ctx {
    let ctx = Ctx::root().unwrap();
    rutis_dsh::provide_agent_aimux(&ctx, service).unwrap();
    let view = ctx.plugin(agent::Plugin::new(agent::Config {
        invariants: Default::default(),
        typert: Default::default(),
        llm: Default::default(),
        session: Default::default(),
        projection: Default::default(),
        system_prompt: Default::default(),
        tools: Default::default(),
        agents: Default::default(),
        agent_loop: Default::default(),
        adapter: agent::AdapterConfig {
            providers: routes
                .into_iter()
                .map(|(name, route)| (name.to_string(), route))
                .collect(),
        },
        llm_probe: Default::default(),
        agent_probe: Default::default(),
    }));
    (&view).await.unwrap();
    ctx
}

fn finish(reason: FinishReasonUnified) -> StreamPart {
    StreamPart::Finish {
        finish_reason: FinishReason {
            unified: reason,
            raw: None,
        },
        usage: Usage::default(),
        provider_metadata: None,
    }
}
