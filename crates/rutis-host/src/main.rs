//! rutis-host: a rutis host that needs no Rust.
//!
//! ```text
//! rutis-host run [rutis.json]      run what the file names
//! rutis-host dev                   run the plugin project here, reloading it as it changes
//! rutis-host check [rutis.json]    describe every row, and fail on what cannot run
//! rutis-host new <name> --lang node|python
//! ```

#[cfg(unix)]
mod config;
#[cfg(unix)]
mod host;
mod new;
#[cfg(unix)]
mod project;
#[cfg(unix)]
mod status;

/// The plugin API this host supports; plugins needing more cannot run here.
const PLUGIN_API: u32 = 1;

const USAGE: &str = "\
rutis-host: run rutis plugins written in TypeScript, JavaScript and Python

usage:
  rutis-host run [rutis.json]               run what the configuration names
  rutis-host dev [dir]                      run the plugin project in dir (default .),
                                            reloading it as its files change
  rutis-host check [rutis.json]             describe every row; fail on what cannot run
                                            (in a plugin project without rutis.json: the project)
  rutis-host new <name> --lang node|python  create a plugin project
  rutis-host --version

Credentials for links come from RUTIS_TOKEN (or RUTIS_TOKEN_<PEER>), RUTIS_CA,
RUTIS_CERT and RUTIS_KEY.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("new") => new_project(&args[1..]),
        Some("--version" | "-V") => {
            println!("rutis-host {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h" | "help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(command @ ("run" | "dev" | "check")) => serve(command, &args[1..]),
        Some(other) => Err(format!("unknown command {other}\n\n{USAGE}")),
    };
    if let Err(error) = result {
        eprintln!("rutis-host: {error}");
        std::process::exit(1);
    }
}

fn new_project(args: &[String]) -> Result<(), String> {
    let mut name = None;
    let mut lang = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lang" => lang = args.next().cloned(),
            other if other.starts_with("--lang=") => {
                lang = Some(other["--lang=".len()..].to_owned())
            }
            other if name.is_none() => name = Some(other.to_owned()),
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    let name = name.ok_or("usage: rutis-host new <name> --lang node|python")?;
    let lang = lang.ok_or("which language? --lang node or --lang python")?;
    new::create(std::path::Path::new("."), &name, &lang)?;
    let next = match lang.as_str() {
        "python" | "py" => {
            "uv sync && uv run python -m unittest discover -s tests && uv run rutis-host dev"
        }
        _ => "npm install && npm test && npx rutis-host dev",
    };
    println!("created {name}/\nnext: cd {name} && {next}");
    Ok(())
}

#[cfg(not(unix))]
fn serve(_: &str, _: &[String]) -> Result<(), String> {
    Err("running plugins needs Linux or macOS (on Windows, use WSL)".into())
}

#[cfg(unix)]
fn serve(command: &str, args: &[String]) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        match command {
            "run" => run(args).await,
            "dev" => dev(args).await,
            _ => check(args).await,
        }
    })
}

#[cfg(unix)]
fn config_file(args: &[String]) -> std::path::PathBuf {
    std::path::PathBuf::from(args.first().map(String::as_str).unwrap_or("rutis.json"))
}

#[cfg(unix)]
async fn run(args: &[String]) -> Result<(), String> {
    let config = config::HostConfig::read(&config_file(args))?;
    let host = host::Host::start(&config).await?;
    host.runtimes_ready().await?;
    host.load(config.rows()).await?;
    status::follow(host.loader.clone());
    // Until Ctrl-C (or a signal) ends the process; the runtimes it started
    // end with their channels.
    std::future::pending::<()>().await;
    Ok(())
}

#[cfg(unix)]
async fn dev(args: &[String]) -> Result<(), String> {
    use std::time::Duration;

    let dir = std::path::PathBuf::from(args.first().map(String::as_str).unwrap_or("."));
    let (config, id) = project::dev_config(&dir)?;
    let host = host::Host::start(&config).await?;
    host.runtimes_ready().await?;
    host.load(config.rows()).await?;
    println!("rutis-host dev: running {id}; changes reload it (Ctrl-C ends)");
    status::follow(host.loader.clone());
    let mut seen = project::sources(&dir);
    loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let now = project::sources(&dir);
        if now == seen {
            continue;
        }
        seen = now;
        // Plugin rows load their new code; peer rows keep their sessions.
        host.invalidate();
        let rows: Vec<String> = host
            .loader
            .entries()
            .into_iter()
            .filter(|entry| entry.options["name"] != "rutis-bridge/peer")
            .map(|entry| entry.id)
            .collect();
        for row in rows {
            match host.loader.reload(&row).await {
                Ok(_) => println!("{row}: reloaded"),
                Err(error) => println!("{row}: cannot reload: {error}"),
            }
        }
    }
}

#[cfg(unix)]
async fn check(args: &[String]) -> Result<(), String> {
    let path = config_file(args);
    let config = match (args.first(), path.exists()) {
        (None, false) => project::dev_config(std::path::Path::new("."))?.0,
        _ => config::HostConfig::read(&path)?,
    };
    let host = host::Host::start(&config).await?;
    host.runtimes_ready().await?;
    println!("plugin API: {PLUGIN_API} (supported by this host)");
    let mut failed = 0;
    for row in config.rows() {
        let id = row["id"].as_str().unwrap_or("?");
        let name = row["name"].as_str().unwrap_or("");
        match host.loader.resolve(name).await {
            Ok(resolved) => {
                println!("{id} ({name}): ok");
                let meta = &resolved.meta;
                if let Some(api) = meta.get("api").and_then(|api| api.as_u64()) {
                    let compatible = if api <= PLUGIN_API as u64 {
                        "compatible"
                    } else {
                        "incompatible: the plugin needs a newer runtime"
                    };
                    println!("  api: {api} ({compatible})");
                }
                for field in ["version", "inject", "provides"] {
                    if let Some(value) = meta.get(field).filter(|value| !value.is_null()) {
                        println!("  {field}: {value}");
                    }
                }
                if let Some(schema) = &resolved.schema {
                    println!("  config: {schema}");
                }
            }
            Err(error) => {
                failed += 1;
                println!("{id} ({name}): {error}");
            }
        }
    }
    match failed {
        0 => Ok(()),
        n => Err(format!("{n} row(s) cannot run")),
    }
}
