//! `rutis-dev <socket> <command> [json-arguments]`: talk to a dev channel.
//!
//! ```text
//! rutis-dev /tmp/host.sock hello
//! rutis-dev /tmp/host.sock load '{"name": "dylib:greeter", "id": "g"}'
//! rutis-dev /tmp/host.sock swap '{"id": "g"}'
//! rutis-dev /tmp/host.sock watch          # streams events until Ctrl-C
//! ```

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(socket), Some(command)) = (args.first(), args.get(1)) else {
        eprintln!("usage: rutis-dev <socket> <hello|describe|status|watch|load|swap|unload-dev> [json-arguments]");
        std::process::exit(2);
    };
    let mut request = match args.get(2) {
        None => serde_json::json!({}),
        Some(text) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(value @ serde_json::Value::Object(_)) => value,
            _ => {
                eprintln!("rutis-dev: arguments must be a JSON object");
                std::process::exit(2);
            }
        },
    };
    request["cmd"] = serde_json::json!(command);
    // `id` is the row argument of load/swap/unload-dev; `req` correlates.
    request["req"] = serde_json::json!(1);
    let stream = match tokio::net::UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("rutis-dev: {socket}: {error}");
            std::process::exit(1);
        }
    };
    let (read, mut write) = stream.into_split();
    if write
        .write_all(format!("{request}\n").as_bytes())
        .await
        .is_err()
    {
        std::process::exit(1);
    }
    let mut lines = BufReader::new(read).lines();
    let mut ok = true;
    while let Ok(Some(line)) = lines.next_line().await {
        let value: serde_json::Value =
            serde_json::from_str(&line).unwrap_or(serde_json::Value::String(line));
        if value.get("ok") == Some(&serde_json::Value::Bool(false)) {
            ok = false;
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        if command != "watch" {
            break;
        }
    }
    std::process::exit(if ok { 0 } else { 1 });
}

#[cfg(not(unix))]
fn main() {
    eprintln!("rutis-dev needs Unix domain sockets");
    std::process::exit(1);
}
