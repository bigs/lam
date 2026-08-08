//! Unix-specific lifecycle checks for the local command runner.

#![cfg(unix)]

use std::time::Duration;

use lam_code::{CaptureConfig, CommandRequest, CommandRunner, LocalCommandRunner};

fn request(
    command: String,
    cwd: &std::path::Path,
    scratch: &std::path::Path,
    timeout: Option<Duration>,
) -> CommandRequest {
    CommandRequest {
        command,
        cwd: cwd.to_path_buf(),
        timeout,
        capture: CaptureConfig::default(),
        scratch_dir: scratch.to_path_buf(),
    }
}

async fn wait_for_marker(path: &std::path::Path, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for readiness marker at {}",
                path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_local_runner_future_kills_background_descendants() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let scratch = tempfile::tempdir().expect("temporary scratch");
    let ready = root.path().join("ready.txt");
    let orphan = root.path().join("orphaned.txt");
    let command = format!(
        "(printf ready > '{}'; sleep 0.3; printf orphaned > '{}') & sleep 1",
        ready.display(),
        orphan.display()
    );
    let task = tokio::spawn(LocalCommandRunner::default().run(request(
        command,
        root.path(),
        scratch.path(),
        None,
    )));
    wait_for_marker(&ready, Duration::from_secs(1)).await;
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !orphan.exists(),
        "background descendant survived cancellation"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn local_runner_omitted_timeout_finishes_normally() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let scratch = tempfile::tempdir().expect("temporary scratch");
    let output = LocalCommandRunner::default()
        .run(request(
            "sleep 0.05; printf done".to_owned(),
            root.path(),
            scratch.path(),
            None,
        ))
        .await
        .expect("unbounded local command");
    assert!(!output.timed_out);
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout.content, "done");
}

#[tokio::test(flavor = "current_thread")]
async fn local_runner_explicit_timeout_kills_process_tree() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let scratch = tempfile::tempdir().expect("temporary scratch");
    let ready = root.path().join("ready.txt");
    let orphan = root.path().join("orphaned.txt");
    let command = format!(
        "(printf ready > '{}'; sleep 0.3; printf orphaned > '{}') & sleep 1",
        ready.display(),
        orphan.display()
    );
    let output = LocalCommandRunner::default()
        .run(request(
            command,
            root.path(),
            scratch.path(),
            // Long enough for readiness; shorter than the delayed orphan write and
            // the foreground shell lifetime.
            Some(Duration::from_millis(150)),
        ))
        .await
        .expect("timed-out local command");
    assert!(output.timed_out);
    assert!(
        ready.exists(),
        "descendant never became ready before explicit timeout"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !orphan.exists(),
        "background descendant survived explicit timeout"
    );
}
