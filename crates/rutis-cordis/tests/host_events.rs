//! M3 端到端:`evt/emit` 通知帧 → `forward_host_events` → 内核 keyed 事件
//! `HostEvent` 的订阅方收到(design-config-hot-update-and-dynamic-events §2.6-9)。
//!
//! 走 MemoryWire(进程内桥),宿主侧用真帧序列驱动;形状与 rutis_dsh runner
//! 的组装一致(forward + stderr 观察者并行)。等待全部用 Notify 信号,
//! 无固定 sleep(评审:CI 高负载下 sleep 等待有假红风险)。

use std::sync::{Arc, Mutex};

use rutis::Listener;
use rutis_cordis::{
    Bridge, BridgeConfig, EventOrigin, ExpectedHost, Frame, HostEvent, InboundHooks, MemoryWire,
    Wire,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

/// 等待条件成立(监听器完成动作后 notify_one;检查先行,permit 不丢失)。
async fn wait_until(cond: impl Fn() -> bool, notify: Arc<Notify>) {
    loop {
        if cond() {
            return;
        }
        notify.notified().await;
    }
}

/// 记录 (name, payload, origin) 的订阅监听器。
struct Recorder {
    seen: Arc<Mutex<Vec<(String, Value, EventOrigin)>>>,
    done: Arc<Notify>,
}

impl Listener<HostEvent> for Recorder {
    fn call<'a>(
        &'a self,
        _ctx: &'a rutis::Ctx,
        e: &'a HostEvent,
    ) -> rutis::BoxFuture<'a, Result<Option<Value>, rutis::CordisError>> {
        let seen = self.seen.clone();
        let done = self.done.clone();
        let name = e.name.clone();
        let payload = e.payload.clone();
        let origin = e.origin.clone();
        Box::pin(async move {
            seen.lock().unwrap().push((name, payload, origin));
            done.notify_one();
            Ok(None)
        })
    }
}

async fn hello_from_host(wire: &MemoryWire) {
    wire.send(Frame::Req {
        id: 1,
        method: "hello".into(),
        params: json!({
            "protocol": 1,
            "base": "cordis",
            "baseSemver": "4.0.1",
            "stack": ["node"],
            "caps": { "services": ["llm"], "wfKinds": [], "scopes": [] },
        }),
        scope_id: None,
        session_id: None,
        turn_id: None,
    })
    .await
    .expect("send hello");
    // 桥的回包(不细验,m1.rs 已锁)
    wire.recv().await.expect("bridge alive for hello res");
}

/// 起一套(桥, 宿主 wire),on_notify = forward_host_events + 计数观察者。
fn assemble(ctx: &rutis::Ctx) -> (Bridge, MemoryWire, Arc<Mutex<Vec<String>>>, Arc<Notify>) {
    let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_done = Arc::new(Notify::new());
    let obs = observed.clone();
    let obs_done = observed_done.clone();
    let hooks = InboundHooks {
        on_request: None,
        on_notify: Some(rutis_cordis::forward_host_events(
            ctx,
            Some(Arc::new(move |method, params, _origin| {
                let obs = obs.clone();
                let obs_done = obs_done.clone();
                Box::pin(async move {
                    if method == "evt/emit" {
                        obs.lock()
                            .unwrap()
                            .push(params["event"].as_str().unwrap_or("?").into());
                        obs_done.notify_one();
                    }
                })
            })),
        )),
    };
    let (bridge_wire, host_wire) = MemoryWire::pair(64);
    let bridge = Bridge::start(
        Box::new(bridge_wire),
        BridgeConfig::default(),
        hooks,
        ExpectedHost::protocol(1),
        json!({ "services": ["llm"], "wfKinds": [], "scopes": [] }),
    );
    (bridge, host_wire, observed, observed_done)
}

#[tokio::test]
async fn host_evt_emit_reaches_keyed_subscriber() {
    let ctx = rutis::Ctx::root().unwrap();
    let seen: Arc<Mutex<Vec<(String, Value, EventOrigin)>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    // 订阅两个名字的宿主事件(运行时字符串)
    ctx.events()
        .on::<HostEvent>(
            &ctx,
            &rutis::EventKey::dynamic("session/event"),
            Recorder {
                seen: seen.clone(),
                done: done.clone(),
            },
        )
        .unwrap();
    let other_seen: Arc<Mutex<Vec<(String, Value, EventOrigin)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let other_done = Arc::new(Notify::new());
    ctx.events()
        .on::<HostEvent>(
            &ctx,
            &rutis::EventKey::dynamic("agent/turn"),
            Recorder {
                seen: other_seen.clone(),
                done: other_done.clone(),
            },
        )
        .unwrap();

    let (mut bridge, host_wire, observed, observed_done) = assemble(&ctx);
    hello_from_host(&host_wire).await;
    bridge.ready().await.expect("handshake");

    // 宿主发 evt/emit:session/event(带 sessionId/turnId —— 三字段透传)
    host_wire
        .send(Frame::Ntf {
            method: "evt/emit".into(),
            params: json!({
                "event": "session/event",
                "params": { "kind": "message", "text": "hi" },
            }),
            scope_id: None,
            session_id: Some("s-1".into()),
            turn_id: Some("t-9".into()),
        })
        .await
        .expect("send evt");
    // 另一个名字(scope 字段)+ 恶形(无 event 字段,应丢弃不进总线)
    host_wire
        .send(Frame::Ntf {
            method: "evt/emit".into(),
            params: json!({
                "event": "agent/turn",
                "params": { "n": 7 },
            }),
            scope_id: Some("scope-a".into()),
            session_id: None,
            turn_id: None,
        })
        .await
        .expect("send evt 2");
    host_wire
        .send(Frame::Ntf {
            method: "evt/emit".into(),
            params: json!({ "params": {} }), // 恶形:无 event
            scope_id: None,
            session_id: None,
            turn_id: None,
        })
        .await
        .expect("send malformed evt");

    // 信号化等待:两个订阅方各收 1 条、观察者收全部 3 帧
    wait_until(|| seen.lock().unwrap().len() == 1, done.clone()).await;
    wait_until(|| other_seen.lock().unwrap().len() == 1, other_done.clone()).await;
    wait_until(
        || observed.lock().unwrap().len() == 3,
        observed_done.clone(),
    )
    .await;

    // 订阅方:按名字精确命中,载荷与 origin 保真
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [(
            "session/event".to_string(),
            json!({ "kind": "message", "text": "hi" }),
            EventOrigin {
                scope_id: None,
                session_id: Some("s-1".to_string()),
                turn_id: Some("t-9".to_string()),
            },
        )],
        "session/event subscriber got its event with origin passthrough"
    );
    assert_eq!(
        other_seen.lock().unwrap().as_slice(),
        [(
            "agent/turn".to_string(),
            json!({ "n": 7 }),
            EventOrigin {
                scope_id: Some("scope-a".to_string()),
                session_id: None,
                turn_id: None,
            },
        )],
        "agent/turn subscriber got its event with origin passthrough"
    );
    // 恶形帧到达观察者(记 "?")但不进事件总线(两个订阅方各恰好 1 条)
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        ["session/event", "agent/turn", "?"]
    );
}

// ── 桥断连:ctx 与事件总线独立存活 ────────────────────────────────

#[tokio::test]
async fn event_bus_survives_bridge_drop() {
    let ctx = rutis::Ctx::root().unwrap();
    let seen: Arc<Mutex<Vec<(String, Value, EventOrigin)>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());
    ctx.events()
        .on::<HostEvent>(
            &ctx,
            &rutis::EventKey::dynamic("after/drop"),
            Recorder {
                seen: seen.clone(),
                done: done.clone(),
            },
        )
        .unwrap();

    let (mut bridge, host_wire, _obs, _obs_done) = assemble(&ctx);
    hello_from_host(&host_wire).await;
    bridge.ready().await.expect("handshake");
    drop(host_wire);
    drop(bridge); // 桥断连/析构

    // ctx 独立于 bridge 存活:emit 直发仍达订阅方
    // (emit 为同步快照 + spawn 尾链,不依赖 bridge)
    ctx.events()
        .emit::<HostEvent>(
            &ctx,
            &rutis::EventKey::dynamic("after/drop"),
            Arc::new(HostEvent {
                name: "after/drop".into(),
                payload: json!({ "ok": true }),
                origin: EventOrigin::default(),
            }),
        )
        .expect("default event dispatch");
    wait_until(|| seen.lock().unwrap().len() == 1, done.clone()).await;
    assert_eq!(seen.lock().unwrap()[0].0, "after/drop");
}
