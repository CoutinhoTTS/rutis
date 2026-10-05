//! A local runtime that cannot start, or ends before it greets, fails with
//! how its process ended, and its handle says so.
#![cfg(unix)]
use rutis::{Ctx, FiberState};
use rutis_interop::{Launcher, RuntimeState};
use rutis_runtime_local::LocalRuntime;

async fn failed_with(launcher: Launcher) -> String {
    let dir = tempfile::tempdir().unwrap();
    let runtime = LocalRuntime::launcher("broken", launcher, dir.path());
    let handle = runtime.handle();
    let root = Ctx::root().unwrap();
    let view = root.plugin(runtime);
    let _ = (&view).await;
    assert_eq!(view.state().state, FiberState::Failed);
    let reason = match handle.state() {
        RuntimeState::Down(reason) => reason,
        _ => panic!("the runtime should be down"),
    };
    root.shutdown().await.unwrap();
    reason
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_that_exits_before_greeting_says_how() {
    let launcher = Launcher::new("sh").arg("-c").arg("exit 17").inherit_fd();
    let reason = failed_with(launcher).await;
    assert!(reason.contains("exited with exit status: 17"), "{reason}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_that_cannot_start_says_why() {
    let reason = failed_with(Launcher::new("/nonexistent/runtime").inherit_fd()).await;
    assert!(
        reason.contains("cannot start /nonexistent/runtime"),
        "{reason}"
    );
}
