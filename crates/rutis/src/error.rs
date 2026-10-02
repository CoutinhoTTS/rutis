use crate::{DependencyStatus, FiberState, InstanceId, PluginId, TypeKey};
use std::panic::Location;
use std::sync::Arc;

/// Why a strict service read was rejected. Optional `get` reads do not use this check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServiceReadFailure {
    Undeclared,
    Unavailable(DependencyStatus),
    OutOfScope,
    Inactive,
    TypeMismatch,
    InterceptDenied,
    InterceptPanicked,
    InterceptReentrant,
}

impl std::fmt::Display for ServiceReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undeclared => f.write_str("dependency not declared"),
            Self::Unavailable(status) => write!(f, "declared dependency unavailable ({status})"),
            Self::OutOfScope => f.write_str("instance outside caller's fiber ancestry"),
            Self::Inactive => f.write_str("caller context inactive"),
            Self::TypeMismatch => f.write_str("service key has a different value type"),
            Self::InterceptDenied => f.write_str("strict read denied by interceptor"),
            Self::InterceptPanicked => f.write_str("strict read interceptor panicked"),
            Self::InterceptReentrant => f.write_str("same-key strict read interceptor reentered"),
        }
    }
}

/// A strict read error with the key, caller identity, and source location.
#[derive(Debug, thiserror::Error)]
#[error("strict service read {key:?} by fiber {plugin_id:?} instance {instance} at {location}: {reason}")]
#[non_exhaustive]
pub struct ServiceReadError {
    pub key: TypeKey,
    pub plugin_id: PluginId,
    pub instance: InstanceId,
    pub location: &'static Location<'static>,
    pub reason: ServiceReadFailure,
}

/// Why a provider-owned mutable service update was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServiceWriteFailure {
    WrongOwner,
    Stale,
    InterceptDenied,
    InterceptPanicked,
    InterceptReentrant,
}

impl std::fmt::Display for ServiceWriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongOwner => f.write_str("writer called outside its provider fiber"),
            Self::Stale => {
                f.write_str("service binding or provider generation is no longer active")
            }
            Self::InterceptDenied => f.write_str("service update denied by interceptor"),
            Self::InterceptPanicked => f.write_str("service update interceptor panicked"),
            Self::InterceptReentrant => {
                f.write_str("same-key service update interceptor reentered")
            }
        }
    }
}

/// A mutable service update error with the original binding identity.
#[derive(Debug, thiserror::Error)]
#[error(
    "service write {key:?} by fiber {provider:?} generation {generation} at {location}: {reason}"
)]
#[non_exhaustive]
pub struct ServiceWriteError {
    pub key: TypeKey,
    pub provider: PluginId,
    pub generation: u64,
    pub location: &'static Location<'static>,
    pub reason: ServiceWriteFailure,
}

/// 框架错误。不 `Clone`(D11);跨任务共享走 `Arc<CordisError>`。
///
/// 变体分层(D25):`PluginFailed` 仅包装 apply/清理边界外来的非 Cordis 错误;
/// apply 自身返回的 `CordisError` 直接传播,不再递归包一层。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CordisError {
    #[error(transparent)]
    ServiceRead(#[from] ServiceReadError),
    #[error("service {0:?} not found in scope")]
    ServiceNotFound(String),
    #[error("plugin failed: {0}")]
    PluginFailed(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("multiple errors: {errors:?}")]
    Aggregate {
        /// 聚合成员,不压平(D18/§五 dispose 契约)。`Arc` 成员是 D11(不
        /// Clone)与 D25(错误 identity 缓存 `Arc`)的推论,见 §八 实现记录。
        errors: Vec<Arc<CordisError>>,
    },
    #[error("fiber disposed")]
    InactiveEffect,
    #[error("stale plugin context: load generation {expected}, current generation {current}")]
    StaleGeneration { expected: u64, current: u64 },
    #[error("plugin context generation {generation} is inactive in state {state:?}")]
    InactiveGeneration { generation: u64, state: FiberState },
    #[error("root has been shut down")]
    Closed,
    #[error("config validation failed: {issues:?}")]
    Validation { issues: Vec<String> },
    /// A dependency is gone. Returned from `apply` after a strict read
    /// (`require` / `require_as`) of a declared dependency found it
    /// unavailable in the same load, it rolls the load back to Pending
    /// instead of Failed; otherwise it is an ordinary load failure.
    #[error("dependency unsatisfied: {0:?}")]
    InjectUnsatisfied(Vec<String>),
    /// 同一 (key, scope) 重复注册(实现新增变体,见 §八 实现记录)。
    #[error("service {0:?} already registered in scope")]
    ServiceExists(String),
    #[error("instance {instance} is outside the caller's fiber subtree")]
    InstanceOutOfScope { instance: InstanceId },
    #[error("synchronous event {key:?} reentered on the same bus")]
    ReentrantEvent { key: TypeKey },
    #[error(transparent)]
    SyncEventPanicked(Arc<CordisError>),
}

/// 错误汇聚点。默认实现输出到 stderr(设计 §二)。
///
/// 成员为 `Arc<CordisError>`:dispose 类错误经 `TransitionTask` 缓存 identity
/// (D25),而 `CordisError` 刻意不 Clone(D11),见 §八 实现记录。
pub type ErrorSink = Arc<dyn Fn(Arc<CordisError>) + Send + Sync>;

pub(crate) fn default_sink() -> ErrorSink {
    Arc::new(|e: Arc<CordisError>| eprintln!("[rutis] {}", format_for_sink(&e)))
}

fn format_for_sink(error: &CordisError) -> String {
    fn write_source(source: &(dyn std::error::Error + 'static), out: &mut String, indent: usize) {
        if let Some(error) = source.downcast_ref::<CordisError>() {
            write_error(error, out, indent);
            return;
        }
        out.push_str(&source.to_string());
        if let Some(next) = source.source() {
            out.push('\n');
            out.push_str(&" ".repeat(indent + 2));
            out.push_str("caused by: ");
            write_source(next, out, indent + 2);
        }
    }

    fn write_error(error: &CordisError, out: &mut String, indent: usize) {
        match error {
            CordisError::Aggregate { errors } => {
                out.push_str("multiple errors:");
                for (index, member) in errors.iter().enumerate() {
                    out.push('\n');
                    out.push_str(&" ".repeat(indent));
                    out.push_str(&format!("{}. ", index + 1));
                    write_error(member, out, indent + 3);
                }
            }
            CordisError::PluginFailed(source) => {
                let display = error.to_string();
                let direct = source.to_string();
                match display.strip_suffix(&direct) {
                    Some(prefix) => {
                        // Keep the public Display as the source of the prefix,
                        // while formatting a nested CordisError structurally.
                        out.push_str(prefix);
                        write_source(source.as_ref(), out, indent);
                    }
                    None => {
                        out.push_str(&display);
                        // A future Display that omits the direct source still
                        // gets its deeper chain without printing it twice.
                        if let Some(next) = source.source() {
                            out.push('\n');
                            out.push_str(&" ".repeat(indent + 2));
                            out.push_str("caused by: ");
                            write_source(next, out, indent + 2);
                        }
                    }
                }
            }
            _ => out.push_str(&error.to_string()),
        }
    }

    let mut out = String::new();
    write_error(error, &mut out, 0);
    out
}

/// 单错原样、多错聚合不压平;返回 `None` 表示无错。
pub(crate) fn aggregate_errors(errors: Vec<CordisError>) -> Option<CordisError> {
    match errors.len() {
        0 => None,
        1 => errors.into_iter().next(),
        _ => Some(CordisError::Aggregate {
            errors: errors.into_iter().map(Arc::new).collect(),
        }),
    }
}

/// 同 [`aggregate_errors`],成员已是共享 identity 的 `Arc`。
pub(crate) fn aggregate_arcs(errors: Vec<Arc<CordisError>>) -> Option<Arc<CordisError>> {
    match errors.len() {
        0 => None,
        1 => errors.into_iter().next(),
        _ => Some(Arc::new(CordisError::Aggregate { errors })),
    }
}

/// 把任务边界捕获的 panic 转成 `PluginFailed`(D30)。
pub(crate) fn panic_error(p: Box<dyn std::any::Any + Send>) -> CordisError {
    let msg = if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic in task".to_string()
    };
    CordisError::PluginFailed(msg.into())
}

/// JoinError → 错误:先区分 panic 与取消——`into_panic()` 在取消场景会
/// 二次 panic(评审 P2),取消转明确的任务取消错误。
pub(crate) fn join_panic_error(join_err: tokio::task::JoinError) -> CordisError {
    if join_err.is_panic() {
        panic_error(join_err.into_panic())
    } else {
        CordisError::PluginFailed("task cancelled before completion".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("outer cause")]
    struct OuterCause {
        #[source]
        source: InnerCause,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("inner cause")]
    struct InnerCause;

    #[test]
    fn plugin_failure_display_and_sink_preserve_each_cause_once() {
        let error = CordisError::PluginFailed(Box::new(OuterCause { source: InnerCause }));
        assert_eq!(error.to_string(), "plugin failed: outer cause");
        assert_eq!(
            format_for_sink(&error),
            "plugin failed: outer cause\n  caused by: inner cause"
        );
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "outer cause"
        );
    }

    #[test]
    fn sink_formats_aggregate_members_without_debug_noise() {
        let error = CordisError::Aggregate {
            errors: vec![
                Arc::new(CordisError::PluginFailed("cleanup detail".into())),
                Arc::new(CordisError::ServiceNotFound("missing".into())),
            ],
        };
        assert_eq!(
            format_for_sink(&error),
            "multiple errors:\n1. plugin failed: cleanup detail\n2. service \"missing\" not found in scope"
        );
    }

    #[test]
    fn sink_recurses_into_aggregate_used_as_plugin_failure_source() {
        let nested = CordisError::Aggregate {
            errors: vec![Arc::new(CordisError::ServiceNotFound("nested".into()))],
        };
        let error = CordisError::PluginFailed(Box::new(nested));
        assert_eq!(
            format_for_sink(&error),
            "plugin failed: multiple errors:\n1. service \"nested\" not found in scope"
        );
    }
}
