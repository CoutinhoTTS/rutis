//! 宿主事件链路(M3,design-config-hot-update-and-dynamic-events §2.4):
//! `evt/emit` 通知帧 → rutis 内核 keyed 事件的翻译缝。
//!
//! 订阅方:`ctx.events().on::<HostEvent>(ctx, &rutis::EventKey::dynamic("session/event"), listener)`;
//! 四分发语义 / fiber 生命周期清理 / once / prepend 全部由内核 keyed 面
//! 免费提供。事件名 = `params.event`(字符串内容匹配,静态/动态互通)。

use std::sync::Arc;

use rutis::{Ctx, EventKey};
use serde_json::Value;

use crate::rpc::{EventOrigin, NotifyHook};

/// 宿主透传事件:`evt/emit` 的翻译产物(D33b:定义在本 crate,内核零 serde)。
///
/// `name` 与注册键的 qualifier 同值——监听器只拿到 `&HostEvent`,看不到键
/// 里的 qualifier,因此 name 存于值内供取用;两份数据由 `forward_host_events`
/// 保证一致。`origin` 是帧的三预留字段(scopeId/sessionId/turnId)透传,
/// 用于区分同事件名下不同会话/回合的来源。`payload` 是宿主事件的原始
/// params。serial/waterfall 的短路值类型为 `serde_json::Value`(宿主侧可短路)。
#[derive(Debug, Clone)]
pub struct HostEvent {
    pub name: String,
    pub payload: Value,
    pub origin: EventOrigin,
}

impl rutis::Event for HostEvent {
    const NAME: &'static str = "host/*";
    type Value = Value;
}

/// 组装 `evt/emit` → `HostEvent` 的入站通知钩子。
///
/// - `evt/emit`(`params.event` 非空字符串):翻译为
///   `emit(ctx, &EventKey::dynamic(name), event)` 转发进内核事件总线。
///   无法接收事件时,错误交给 ErrorSink。
///   恶形(无 event / 非字符串)丢弃并 `eprintln!` 一行截断摘要——通知帧
///   无回执通道,错误无处上报,但不可静默消失,也不可被巨型载荷打爆日志
///   (防御纵深)。
/// - 所有通知帧(Ntf):原样交给 `observe`(可选观察者,如 stderr 摘要
///   日志)。**转发先于 observe** 且互不隔离:observe panic/阻塞不吞事件
///   (emit 只入队尾链任务,不等待派发完成)。
pub fn forward_host_events(ctx: &Ctx, observe: Option<NotifyHook>) -> NotifyHook {
    let ctx = ctx.clone();
    Arc::new(move |method, params, origin| {
        let ctx = ctx.clone();
        let observe = observe.clone();
        Box::pin(async move {
            if method == "evt/emit" {
                if let Some(name) = params.get("event").and_then(Value::as_str) {
                    let payload = params.get("params").cloned().unwrap_or(Value::Null);
                    // 先转发后观察:转发是主线,observe 是诊断辅助。
                    if let Err(error) = ctx.events().emit::<HostEvent>(
                        &ctx,
                        &EventKey::dynamic(name),
                        Arc::new(HostEvent {
                            name: name.to_string(),
                            payload,
                            origin: origin.clone(),
                        }),
                    ) {
                        ctx.error_sink()(Arc::new(error));
                    }
                } else {
                    let summary = serde_json::to_string(&params).unwrap_or_else(|_| "?".into());
                    eprintln!(
                        "[rutis-cordis] malformed evt/emit dropped (no string event field) \
                         origin={origin:?}: {}",
                        truncate(&summary, 200)
                    );
                }
            }
            if let Some(observe) = observe {
                observe(method, params, origin).await;
            }
        })
    })
}

/// 恶形帧摘要截断:按字符数,超长以 `…` 结尾(防御巨型载荷刷爆 stderr)。
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}
