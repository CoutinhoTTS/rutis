//! dsh-llm callers stream model calls served by aimux-llm in the rutis host.
#![cfg(all(unix, dsh_llm))]

mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use common::{mounted, Scripted};
use dsh_llm_mount::dsh;
use serde_json::{json, Value};

fn chunks(raw: Vec<String>) -> Vec<Value> {
    raw.iter()
        .map(|chunk| serde_json::from_str(chunk).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn dsh_streams_text_tool_calls_and_usage_from_aimux() {
    let service = Arc::new(Scripted::default());
    let ctx = mounted(service.clone()).await;
    let probe = ctx.get::<dsh::LlmProbe>().unwrap();

    let got = chunks(
        probe
            .collect("scripted", "m", "be brief", "say hello")
            .await
            .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            json!({ "type": "block-start", "index": 0, "blockType": "text" }),
            json!({ "type": "text-delta", "index": 0, "text": "hel" }),
            json!({ "type": "text-delta", "index": 0, "text": "lo" }),
            json!({ "type": "block-end", "index": 0, "block": { "type": "text", "text": "hello" } }),
            json!({ "type": "block-start", "index": 1, "blockType": "tool-call" }),
            json!({ "type": "tool-call-delta", "index": 1, "id": "call-1", "name": "lookup", "argumentsDelta": "{\"q\":\"m2\"}" }),
            json!({ "type": "block-end", "index": 1, "block": { "type": "tool-call", "id": "call-1", "name": "lookup", "arguments": "{\"q\":\"m2\"}" } }),
            json!({ "type": "usage", "usage": { "inputTokens": 10, "outputTokens": 5, "cacheReadTokens": 2 } }),
            json!({ "type": "finish", "reason": { "kind": "tool-calls" } }),
        ]
    );

    let requests = service.requests.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.provider.as_deref(), Some("scripted"));
    assert_eq!(request.model.as_deref(), Some("m"));
    assert_eq!(request.options.system.as_deref(), Some("be brief"));
    assert_eq!(request.options.messages.len(), 1);
    assert_eq!(request.options.messages[0].role.as_deref(), Some("user"));
    assert_eq!(request.options.messages[0].text, "say hello");
    assert_eq!(request.options.tools[0].name, "lookup");
    assert_eq!(
        request.options.tools[0].parameters,
        Some(json!({ "type": "object" }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn aimux_failures_end_the_dsh_stream_with_their_code() {
    let ctx = mounted(Arc::new(Scripted::default())).await;
    let probe = ctx.get::<dsh::LlmProbe>().unwrap();

    let got = chunks(probe.collect("failing", "m", "", "hi").await.unwrap());
    assert_eq!(
        got,
        vec![json!({
            "type": "finish",
            "reason": { "kind": "error", "failure": { "code": "AUTH", "message": "no key for this provider" } },
        })]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dsh_caller_that_stops_reading_stops_the_aimux_stream() {
    let service = Arc::new(Scripted::default());
    let ctx = mounted(service.clone()).await;
    let probe = ctx.get::<dsh::LlmProbe>().unwrap();

    let first: Value =
        serde_json::from_str(&probe.first_chunk("endless", "m").await.unwrap()).unwrap();
    assert_eq!(first["type"], "block-start");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !service.dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the aimux stream is dropped once the caller stops");
}

#[tokio::test(flavor = "multi_thread")]
async fn dsh_lists_models_from_aimux() {
    let ctx = mounted(Arc::new(Scripted::default())).await;
    let runtime = ctx.get::<dsh::LlmRuntime>().unwrap();
    let models = runtime.list_models("scripted").await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "scripted-chat");
}

#[tokio::test(flavor = "multi_thread")]
async fn routes_name_their_aimux_provider_key_and_display_name() {
    // The Node process inherits this environment; no credentials service is mounted.
    std::env::set_var("DSH_LLM_MOUNT_TEST_KEY", "sk-test");
    let service = Arc::new(Scripted::default());
    let ctx = common::mount_routes(
        service.clone(),
        [(
            "fast",
            dsh::Route {
                provider: Some("scripted".into()),
                api_key_env: Some("DSH_LLM_MOUNT_TEST_KEY".into()),
                display_name: Some("Fast (aimux)".into()),
            },
        )],
    )
    .await;

    let runtime = ctx.get::<dsh::LlmRuntime>().unwrap();
    let providers = runtime.list_providers().unwrap();
    let fast = providers
        .iter()
        .find(|provider| provider.id == "fast")
        .unwrap();
    assert_eq!(fast.name, "Fast (aimux)");

    let probe = ctx.get::<dsh::LlmProbe>().unwrap();
    let got = chunks(probe.collect("fast", "m", "", "hi").await.unwrap());
    assert_eq!(got.last().unwrap()["type"], "finish");
    let request = service.requests.lock().unwrap()[0].clone();
    assert_eq!(request.provider.as_deref(), Some("scripted"));
    assert_eq!(request.api_key.as_deref(), Some("sk-test"));

    let models = runtime.list_models("fast").await.unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["scripted-chat"]
    );
    assert_eq!(
        service.listed.lock().unwrap().as_slice(),
        [("scripted".to_string(), Some("sk-test".to_string()))]
    );
}
