//! 最小示例:一个提供服务的插件,一个依赖它的插件,一次替换 provider。
//!
//! 运行:`cargo run -p rutis --example quickstart`
//!
//! 预期输出:
//! ```text
//! hello from English
//! hello from Esperanto
//! ```
//!
//! 第二行是 rutis 的核心能力在起作用:没有人碰过 `Listener`,
//! provider 换代(旧的卸载、新的提供)驱动它停下并重新装载。

use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, Typed, TypedPlugin};

/// 服务就是一个类型。
struct Greeting(String);

/// 提供 `Greeting`。apply 里注册的东西,插件停下时自动释放。
struct Greeter(&'static str);

impl Plugin for Greeter {
    fn name(&self) -> &str {
        "greeter"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide(Greeting(format!("hello from {}", self.0)))?;
            Ok(Effect::Done)
        })
    }
}

/// 依赖 `Greeting`:它出现时启动,被替换时重启。
struct Listener;

impl TypedPlugin for Listener {
    type Deps = (Arc<Greeting>,);

    fn name(&self) -> &str {
        "listener"
    }

    fn apply<'a>(
        &'a self,
        _: &'a Ctx,
        (greeting,): Self::Deps,
    ) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            println!("{}", greeting.0);
            Ok(Effect::Done)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ctx = Ctx::root()?;
    let listener = ctx.plugin(Typed::new(Listener)); // 等待 Greeting

    let english = ctx.plugin(Greeter("English"));
    (&english).await?;
    (&listener).await?; // hello from English

    english.dispose().await?; // listener 随之停下……
    let esperanto = ctx.plugin(Greeter("Esperanto"));
    (&esperanto).await?;
    (&listener).await?; // ……又自动启动:hello from Esperanto

    ctx.shutdown().await?;
    Ok(())
}
