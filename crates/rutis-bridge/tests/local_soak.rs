//! Soak: processes started, talked to and ended over and over through
//! `spawn:`. Every process must be reaped, and what this process holds (file
//! descriptors, threads) must not grow with their number. Ignored by
//! default; run it with
//! `RUTIS_SOAK_SECS=600 cargo test -p rutis-bridge --test local_soak -- --ignored`
//! (60 s when unset). Linux only: it reads `/proc`.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use rutis_bridge::channel::PeerId;
use rutis_bridge::transport::local::{LocalTransport, Spawn};
use rutis_bridge::{Dial, Transport};

/// Open file descriptors and threads of this process.
fn held() -> (usize, usize) {
    let fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
    let threads = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (fds, threads)
}

/// Children of this process not yet reaped, zombies included.
fn children() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("children")).ok())
        .map(|children| children.split_whitespace().count())
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "soak: run with --ignored, RUTIS_SOAK_SECS sets the duration"]
async fn starting_processes_over_and_over_holds_nothing_more() {
    let duration = Duration::from_secs(
        std::env::var("RUTIS_SOAK_SECS")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .unwrap_or(60),
    );
    let transport = LocalTransport::default();
    let mut echo = Spawn::new("sh", PeerId::new("child").unwrap());
    echo.args = vec!["-c".into(), r#"read line <&3; echo "$line" >&3"#.into()];
    transport.spawner("echo", echo);
    // Half of them are closed by this side before they answer: they end
    // after the grace period or by themselves.
    let mut idle = Spawn::new("sh", PeerId::new("idle").unwrap());
    idle.args = vec!["-c".into(), "read line <&3".into()];
    transport.spawner("idle", idle);

    let started = Instant::now();
    let mut baseline = None;
    let mut processes = 0u64;
    while started.elapsed() < duration {
        let mut channel = transport.dial(&Dial::address("spawn:echo")).await.unwrap();
        channel.sender.send(b"ping").unwrap();
        let reply = tokio::task::spawn_blocking(move || {
            let reply = channel.receiver.recv();
            (reply, channel)
        })
        .await
        .unwrap();
        assert_eq!(reply.0.unwrap().as_deref(), Some(&b"ping"[..]));
        drop(reply.1);

        let idle = transport.dial(&Dial::address("spawn:idle")).await.unwrap();
        idle.closer.close("soak");
        drop(idle);
        processes += 2;
        if processes == 40 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            baseline = Some(held());
        }
    }
    // Every process ended and was reaped.
    let deadline = Instant::now() + Duration::from_secs(10);
    while children() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (fds, threads) = held();
    let (base_fds, base_threads) = baseline.expect("enough processes to measure");
    eprintln!(
        "soak: {processes} processes in {:?}; fds {base_fds} -> {fds}, threads {base_threads} -> {threads}, children left {}",
        started.elapsed(),
        children()
    );
    assert_eq!(children(), 0, "every process is reaped");
    assert!(
        fds <= base_fds + 16,
        "file descriptors grew: {base_fds} -> {fds}"
    );
    assert!(
        threads <= base_threads + 16,
        "threads grew: {base_threads} -> {threads}"
    );
}
