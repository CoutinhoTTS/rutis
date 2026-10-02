//! A dsh agent loop mounted in rutis runs a whole turn on models served by aimux.
#![cfg(all(unix, dsh_installed))]

mod common;

use std::sync::Arc;

use common::{mounted, Scripted};
use rutis_dsh::agent;
use serde_json::{json, Value};

#[tokio::test(flavor = "multi_thread")]
async fn a_dsh_agent_turn_calls_a_tool_and_answers_through_aimux() {
    let service = Arc::new(Scripted::default());
    let ctx = mounted(service.clone()).await;
    let probe = ctx.get::<agent::AgentProbe>().unwrap();

    let messages: Vec<Value> = probe
        .run("agent", "m", "What is the code name?")
        .await
        .unwrap()
        .iter()
        .map(|message| serde_json::from_str(message).unwrap())
        .collect();
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "tool", "assistant"],
        "{messages:#?}"
    );
    assert_eq!(
        messages[1]["content"],
        json!([{ "type": "text", "text": "What is the code name?" }])
    );
    assert_eq!(
        messages[2]["content"],
        json!([{ "type": "tool-call", "id": "call-1", "name": "lookup", "arguments": "{\"q\":\"m2\"}" }])
    );
    // dsh ran the tool itself and fed the result back to the model.
    assert_eq!(
        messages[3]["content"],
        json!([{ "type": "text", "text": "RESULT:m2" }])
    );
    assert_eq!(messages[3]["isError"], json!(false));
    assert_eq!(
        messages[4]["content"],
        json!([{ "type": "text", "text": "The code is RESULT:m2." }])
    );

    // aimux served both model requests of the turn.
    let requests = service.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]
            .options
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["lookup"]
    );
    // The second request carries the turn's tool round trip, not flattened text.
    let history = &requests[1].options.messages;
    let call = history
        .iter()
        .find(|message| message.role.as_deref() == Some("assistant"))
        .unwrap();
    let calls: Vec<_> = call
        .tool_calls
        .iter()
        .map(|call| {
            (
                call.id.as_str(),
                call.name.as_str(),
                call.arguments.as_str(),
            )
        })
        .collect();
    assert_eq!(calls, [("call-1", "lookup", "{\"q\":\"m2\"}")]);
    let result = history.last().unwrap();
    assert_eq!(result.role.as_deref(), Some("tool"));
    assert_eq!(result.text, "RESULT:m2");
    assert_eq!(result.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(result.tool_name.as_deref(), Some("lookup"));
    assert_eq!(result.is_error, Some(false));
}

/// The same turn against a real provider, configured like `rutis-dsh`:
/// `DEEPSEEK_API_KEY` (or `AIMUX_PROVIDER`, `AIMUX_MODEL` and that provider's
/// key). Run with `cargo test -p rutis-dsh --test agent -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "calls a real model provider"]
async fn a_dsh_agent_turn_on_a_real_model() {
    let provider = std::env::var("AIMUX_PROVIDER").unwrap_or_else(|_| "deepseek".into());
    let model = std::env::var("AIMUX_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
    let ctx = common::mount(Arc::new(aimux_llm::AimuxLlm::from_env()), &[&provider]).await;
    let probe = ctx.get::<agent::AgentProbe>().unwrap();

    let messages = probe
        .run(
            &provider,
            &model,
            "Call the lookup tool with q set to \"m2\", then tell me exactly what it returned.",
        )
        .await
        .unwrap();
    for message in &messages {
        eprintln!("{message}");
    }
    let messages: Vec<Value> = messages
        .iter()
        .map(|message| serde_json::from_str(message).unwrap())
        .collect();
    assert!(
        messages.iter().any(
            |message| message["role"] == "tool" && message["content"][0]["text"] == "RESULT:m2"
        ),
        "the model did not call the tool"
    );
    let answer = messages.last().unwrap();
    assert_eq!(answer["role"], "assistant");
    assert!(
        answer["content"].to_string().contains("RESULT:m2"),
        "{answer}"
    );
}
