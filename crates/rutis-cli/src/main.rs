//! rutis-cli——最小 coding agent 的命令行形态(minimal mode)。
//!
//! ```text
//! export DEEPSEEK_API_KEY=... && rutis-cli                    # deepseek-chat
//! rutis-cli --provider ollama --model qwen3:8b                # 本地模型
//! AIMUX_PROVIDER=ollama AIMUX_MODEL=qwen3:8b rutis-cli        # 环境变量等价
//! rutis-cli --scripted                                        # 无 key 离线演示
//! ```
//!
//! 工具集 = `bash` + `replace_text`(能改文件、能跑命令);交互见 TUI 界面
//! 底栏:Enter 提交,Esc / Ctrl+C(运行中)取消当前 turn,Ctrl+q 退出。
//! session 默认持久化到 `<cwd>/.rutis/session.json`,重启恢复历史。

use std::sync::Arc;

use aimux_core::language_model::LanguageModel;
use rutis::Ctx;
#[cfg(all(feature = "dylib-plugins", panic = "abort"))]
compile_error!("dylib-plugins requires panic = unwind");
use rutis_agent::{
    llm_key, minimal_persona, minimal_tools, AgentDriverPlugin, ToolsPlugin, TuiPlugin,
};
#[cfg(feature = "dylib-plugins")]
use rutis_sdk as _;

const USAGE: &str = "\
rutis-cli — minimal coding agent (bash + replace_text) on the rutis framework

USAGE:
    rutis-cli [OPTIONS]

OPTIONS:
    -p, --provider <ID>   aimux provider id [env: AIMUX_PROVIDER] [default: deepseek]
    -m, --model <ID>      model id [env: AIMUX_MODEL] [default: deepseek-chat]
        --scripted        offline demo backend (no API key needed)
        --plugin <DIR>    load a trusted dylib plugin [dylib-plugins build only]
        --plugin-config <JSON>   configuration passed to the dylib plugin
        --load-only       with --plugin: load, apply and exit (no TUI) [dylib-plugins build only]
    -h, --help            print this help
    -V, --version         print the version
        --sdk-info        print dylib SDK build identity [dylib-plugins build only]
";

#[cfg(feature = "dylib-plugins")]
fn main() {
    // The launcher clears LD_* (DYLD_* on macOS) while starting this host.
    // Restore the caller's values before starting runtime threads so child
    // commands inherit them. The dynamic loader has read its variables by now.
    restore_bundle_environment();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(cli_main());
}

#[cfg(not(feature = "dylib-plugins"))]
#[tokio::main]
async fn main() {
    cli_main().await;
}

#[cfg(feature = "dylib-plugins")]
fn restore_bundle_environment() {
    if std::env::var_os("RUTIS_DYLIB_LAUNCHER").is_none() {
        return;
    }
    std::env::remove_var("RUTIS_DYLIB_LAUNCHER");
    #[cfg(target_os = "macos")]
    const LOADER_PREFIX: &str = "DYLD_";
    #[cfg(not(target_os = "macos"))]
    const LOADER_PREFIX: &str = "LD_";
    // The Linux launcher sets LD_LIBRARY_PATH to the bundle. The Windows
    // launcher changes no variable and saves none, so nothing below applies.
    #[cfg(target_os = "linux")]
    std::env::remove_var("LD_LIBRARY_PATH");
    let originals = std::env::vars_os()
        .filter_map(|(name, value)| {
            name.to_str()
                .and_then(|name| name.strip_prefix("RUTIS_ORIG_"))
                .filter(|original| original.starts_with(LOADER_PREFIX))
                .map(|original| (name.clone(), original.to_owned(), value))
        })
        .collect::<Vec<_>>();
    for (saved_name, original_name, value) in originals {
        std::env::set_var(original_name, value);
        std::env::remove_var(saved_name);
    }
}

#[cfg(all(test, feature = "dylib-plugins"))]
mod bundle_env_tests {
    use super::restore_bundle_environment;
    use std::ffi::OsString;

    fn put(name: &str, value: Option<OsString>) {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn child_environment_recovers_callers_loader_values() {
        let p = if cfg!(target_os = "macos") {
            "DYLD_"
        } else {
            "LD_"
        };
        let library_path = format!("{p}LIBRARY_PATH");
        let other = format!("{p}PRELOAD");
        let saved_library_path = format!("RUTIS_ORIG_{library_path}");
        let saved_other = format!("RUTIS_ORIG_{other}");
        let names = [
            "RUTIS_DYLIB_LAUNCHER".to_owned(),
            saved_library_path.clone(),
            saved_other.clone(),
            library_path.clone(),
            other.clone(),
        ];
        let saved = names.clone().map(|name| std::env::var_os(name));
        std::env::set_var("RUTIS_DYLIB_LAUNCHER", "1");
        std::env::set_var(&saved_library_path, "/caller/libs");
        std::env::set_var(&saved_other, "/caller/preload");
        std::env::set_var(&library_path, "/verified/bundle");
        restore_bundle_environment();
        assert_eq!(std::env::var(&library_path).unwrap(), "/caller/libs");
        assert_eq!(std::env::var(&other).unwrap(), "/caller/preload");
        assert!(std::env::var_os(&saved_library_path).is_none());
        for (name, value) in names.iter().zip(saved) {
            put(name, value);
        }
    }
}

async fn cli_main() {
    let mut provider = std::env::var("AIMUX_PROVIDER").unwrap_or_else(|_| "deepseek".into());
    let mut model = std::env::var("AIMUX_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
    let mut scripted = false;
    #[cfg(feature = "dylib-plugins")]
    let mut load_only = false;
    #[cfg(not(feature = "dylib-plugins"))]
    let load_only = false;
    let mut plugin: Option<String> = None;
    let mut plugin_config = serde_json::Value::Null;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-p" | "--provider" => provider = value(&mut args, &arg),
            "-m" | "--model" => model = value(&mut args, &arg),
            "--scripted" => scripted = true,
            #[cfg(feature = "dylib-plugins")]
            "--load-only" => load_only = true,
            #[cfg(not(feature = "dylib-plugins"))]
            "--load-only" => {
                eprintln!("--load-only requires a dylib-plugins build");
                std::process::exit(2);
            }
            #[cfg(feature = "dylib-plugins")]
            "--sdk-info" => {
                println!("[sdk]\nversion = {:?}\nid = {:?}\nartifact_sha256 = {:?}\ntarget = {:?}\nrustc = {:?}",
                    rutis_sdk::SDK_VERSION,
                    rutis_sdk::SDK_ID,
                    option_env!("RUTIS_SDK_ARTIFACT_SHA256").unwrap_or(""),
                    rutis_sdk::SDK_TARGET,
                    rutis_sdk::SDK_RUSTC_VERSION,
                );
                println!("packages = [");
                for package in rutis_sdk::SDK_PACKAGES {
                    println!("  {package:?},");
                }
                println!("]");
                return;
            }
            "--plugin" => plugin = Some(value(&mut args, &arg)),
            "--plugin-config" => {
                let raw = value(&mut args, &arg);
                plugin_config = serde_json::from_str(&raw).unwrap_or_else(|error| {
                    eprintln!("invalid --plugin-config JSON: {error}");
                    std::process::exit(2);
                });
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            "-V" | "--version" => {
                println!("rutis-cli {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            other => {
                eprintln!("unknown argument: {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let llm: Arc<dyn LanguageModel> = if scripted {
        Arc::new(rutis_agent::ScriptedLlm::new(scripted_responses()))
    } else {
        match aimux_providers::provider(&provider, None, &model, None) {
            Ok(m) => Arc::from(m),
            Err(e) => {
                eprintln!("failed to build {provider}/{model}: {e}");
                if provider == "deepseek" && std::env::var_os("DEEPSEEK_API_KEY").is_none() {
                    eprintln!(
                        "hint: export DEEPSEEK_API_KEY=... ; or offline demo: rutis-cli --scripted"
                    );
                }
                std::process::exit(1);
            }
        }
    };
    #[cfg(feature = "dylib-plugins")]
    if load_only && plugin.is_none() {
        eprintln!("--load-only needs --plugin <DIR> (there is nothing to load)");
        std::process::exit(2);
    }
    let model_id = if scripted {
        "scripted".to_string()
    } else {
        model.clone()
    };

    if let Err(e) = run(llm, &provider, &model_id, plugin, plugin_config, load_only).await {
        eprintln!("rutis-cli failed: {e}");
        std::process::exit(1);
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next()
        .unwrap_or_else(|| {
            eprintln!("missing value for {flag}\n\n{USAGE}");
            std::process::exit(2);
        })
        .trim_matches('"')
        .to_string()
}

async fn run(
    llm: Arc<dyn LanguageModel>,
    provider: &str,
    model: &str,
    plugin: Option<String>,
    plugin_config: serde_json::Value,
    load_only: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());
    #[cfg(feature = "dylib-plugins")]
    let loader = rutis_dylib::Loader::new(
        option_env!("RUTIS_SDK_ARTIFACT_SHA256").unwrap_or(""),
        std::path::Path::new(&cwd).join(".rutis/plugin-cache"),
        std::collections::HashMap::new(),
        4,
    )?;
    #[cfg(not(feature = "dylib-plugins"))]
    if plugin.is_some() {
        return Err("--plugin requires a dylib-plugins build".into());
    }
    let root = Ctx::root()?;
    root.provide_as(llm_key(), llm)?;
    #[cfg(feature = "dylib-plugins")]
    let plugin_view = if let Some(dir) = plugin {
        let module = unsafe { loader.load(dir)? };
        let view = loader.spawn(&root, &module, plugin_config)?;
        (&view).await?;
        if load_only {
            // A plugin verification mode: load, construct and apply, then
            // exit without starting the TUI. The TUI main loop waits for
            // input, which hangs on consoles without one (CI).
            return Ok(());
        }
        Some(view)
    } else {
        None
    };
    #[cfg(not(feature = "dylib-plugins"))]
    let _ = (plugin_config, load_only);

    // session 持久化(默认 <cwd>/.rutis/session.json,重启恢复历史)
    let tools_view = root.plugin(ToolsPlugin::new(minimal_tools()));
    let driver_view = root.plugin(
        AgentDriverPlugin::new(10000)
            .with_system_prompt(minimal_persona(model, &cwd))
            .with_default_session_path(),
    );

    (&tools_view).await?;
    (&driver_view).await?;

    // TUI 在 driver 装载完成后创建:apply 内 get agent 必成功(启动门控)
    let tui_view = root.plugin(TuiPlugin::new().with_intro(vec![
        format!("backend: {provider}/{model}"),
        format!("cwd: {cwd}"),
        "tools: bash + replace_text | Enter 发送 | Esc 取消 | Ctrl+Q 退出".to_string(),
    ]));
    // TUI apply 即主循环:退出(或 fiber 卸载)后 settle 才完成
    let _ = (&tui_view).await;

    // 卸载 TUI fiber 后级联收尾
    tui_view.dispose().await?;
    driver_view.dispose().await?;
    tools_view.dispose().await?;
    #[cfg(feature = "dylib-plugins")]
    if let Some(view) = plugin_view {
        view.dispose().await?;
    }

    Ok(())
}

/// 离线演示脚本:一轮工具调用(建文件 + cat)+ 一轮终答。
fn scripted_responses() -> Vec<rutis_agent::LlmResponse> {
    use aimux_core::tool::ToolCall;
    use serde_json::json;

    let mk = |id: &str, name: &str, input: serde_json::Value| ToolCall {
        tool_call_id: id.into(),
        tool_name: name.into(),
        input,
        provider_executed: None,
        dynamic: None,
        thought_signature: None,
    };
    vec![
        rutis_agent::LlmResponse::tool_calls(vec![
            mk(
                "c1",
                "replace_text",
                json!({
                    "command": "create",
                    "path": "rutis-cli-demo.txt",
                    "file_text": "hello from the scripted backend\n"
                }),
            ),
            mk(
                "c2",
                "bash",
                json!({
                    "command": "cat rutis-cli-demo.txt",
                    "description": "Show the file just created"
                }),
            ),
        ]),
        rutis_agent::LlmResponse::content(
            "demo done: created rutis-cli-demo.txt and read it back. Ask me to edit real files with a real backend key.",
        ),
    ]
}
