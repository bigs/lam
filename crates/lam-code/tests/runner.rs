//! Unix-specific lifecycle checks for the local command runner.

#![cfg(unix)]

use std::time::Duration;

use lam_code::{CaptureConfig, CommandRequest, CommandRunner, LocalCommandRunner};

#[tokio::test(flavor = "current_thread")]
async fn dropping_local_runner_future_kills_background_descendants() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let scratch = tempfile::tempdir().expect("temporary scratch");
    let marker = root.path().join("orphaned.txt");
    let command = format!("(sleep 0.3; printf orphaned > '{}') &", marker.display());
    let task = tokio::spawn(LocalCommandRunner::default().run(CommandRequest {
        command,
        cwd: root.path().to_path_buf(),
        timeout: Duration::from_secs(2),
        capture: CaptureConfig::default(),
        scratch_dir: scratch.path().to_path_buf(),
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !marker.exists(),
        "background descendant survived cancellation"
    );
}
