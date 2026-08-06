//! End-to-end checks for the coding capability namespaces.

use std::time::Duration;

use lam::{EvalError, EvalValue, Isolate};
use lam_code::{
    CaptureConfig, CodingPack, FilesystemAccess, ListConfig, LocalCommandRunner, ReadConfig,
    ShellConfig,
};
use serde_json::{Value, json};

fn json_result(value: EvalValue) -> Value {
    match value {
        EvalValue::Json(value) => value,
        EvalValue::Undefined => panic!("expected a JSON evaluation result"),
    }
}

async fn isolate(pack: &CodingPack) -> Isolate {
    Isolate::builder()
        .namespaces(pack)
        .build()
        .await
        .expect("coding namespaces should build")
}

#[test]
fn coding_pack_installs_only_configured_namespaces() {
    let root = tempfile::tempdir().expect("temporary workspace");

    let default = CodingPack::builder(root.path())
        .build()
        .expect("default pack");
    assert_eq!(
        default
            .namespaces()
            .map(|namespace| namespace.path().to_owned())
            .collect::<Vec<_>>(),
        ["lam.fs", "lam.edit"]
    );

    let read_only = CodingPack::builder(root.path())
        .filesystem_access(FilesystemAccess::ReadOnly)
        .build()
        .expect("read-only pack");
    assert_eq!(
        read_only
            .namespaces()
            .map(|namespace| namespace.path().to_owned())
            .collect::<Vec<_>>(),
        ["lam.fs"]
    );

    let shell_only = CodingPack::builder(root.path())
        .filesystem_access(FilesystemAccess::Disabled)
        .shell(LocalCommandRunner::default())
        .build()
        .expect("shell-only pack");
    assert_eq!(
        shell_only
            .namespaces()
            .map(|namespace| namespace.path().to_owned())
            .collect::<Vec<_>>(),
        ["lam.shell"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn read_numbers_and_paginates_while_list_uses_a_lexical_cursor() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(root.path().join("sample.txt"), "one\ntwo\nthree\nfour\n")
        .expect("write sample");
    let directory = root.path().join("entries");
    std::fs::create_dir(&directory).expect("create directory");
    std::fs::write(directory.join("c.txt"), "ccc").expect("write c");
    std::fs::write(directory.join("a.txt"), "a").expect("write a");
    std::fs::create_dir(directory.join("b-dir")).expect("create b directory");

    let pack = CodingPack::builder(root.path())
        .filesystem_access(FilesystemAccess::ReadOnly)
        .read_config(ReadConfig {
            default_lines: 2,
            max_lines: 4,
            max_bytes: 128,
        })
        .list_config(ListConfig {
            default_entries: 2,
            max_entries: 4,
        })
        .build()
        .expect("read-only pack");
    let mut isolate = isolate(&pack).await;

    let manifest = isolate
        .eval("lam.dir({ path: 'lam.fs.read' })")
        .await
        .expect("read capability should be discoverable");
    let manifest = json_result(manifest.result);
    assert_eq!(manifest[0]["path"], "lam.fs");
    assert_eq!(manifest[0]["functions"][0]["name"], "read");
    assert!(
        manifest[0]["functions"][0]["docs"]
            .as_str()
            .is_some_and(|docs| docs.contains("one-indexed line numbers"))
    );
    assert!(manifest[0]["functions"][0]["inputSchema"]["properties"]["offset"].is_object());

    let first = isolate
        .eval("await lam.fs.read({ path: 'sample.txt' })")
        .await
        .expect("first read page");
    assert_eq!(
        json_result(first.result),
        json!({
            "path": "sample.txt",
            "startLine": 1,
            "endLine": 2,
            "content": "1\tone\n2\ttwo",
            "nextOffset": 3
        })
    );

    let second = isolate
        .eval("await lam.fs.read({ path: 'sample.txt', offset: 3 })")
        .await
        .expect("second read page");
    assert_eq!(
        json_result(second.result),
        json!({
            "path": "sample.txt",
            "startLine": 3,
            "endLine": 4,
            "content": "3\tthree\n4\tfour"
        })
    );

    let first_listing = isolate
        .eval("await lam.fs.list({ path: 'entries' })")
        .await
        .expect("first listing page");
    assert_eq!(
        json_result(first_listing.result),
        json!({
            "path": "entries",
            "entries": [
                { "name": "a.txt", "kind": "file", "sizeBytes": 1 },
                { "name": "b-dir", "kind": "directory" }
            ],
            "nextAfter": "b-dir"
        })
    );

    let second_listing = isolate
        .eval("await lam.fs.list({ path: 'entries', after: 'b-dir' })")
        .await
        .expect("second listing page");
    assert_eq!(
        json_result(second_listing.result),
        json!({
            "path": "entries",
            "entries": [
                { "name": "c.txt", "kind": "file", "sizeBytes": 3 }
            ]
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn patch_validates_every_operation_before_committing_and_supports_file_actions() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(root.path().join("update.txt"), "alpha\nbeta\ngamma\n")
        .expect("write update fixture");
    std::fs::write(root.path().join("delete.txt"), "obsolete\n").expect("write delete fixture");
    std::fs::write(root.path().join("move.txt"), "old\n").expect("write move fixture");

    let pack = CodingPack::builder(root.path())
        .build()
        .expect("coding pack");
    let mut isolate = isolate(&pack).await;
    let patch = r"*** Begin Patch
*** Update File: update.txt
@@
 alpha
-beta
+BETA
 gamma
*** Add File: nested/new.txt
+new
*** Delete File: delete.txt
*** Update File: move.txt
*** Move to: moved.txt
@@
-old
+new
*** End Patch";
    let source = format!(
        "await lam.edit.apply({{ patch: {} }})",
        serde_json::to_string(patch).expect("serialize patch")
    );
    let applied = isolate.eval(&source).await.expect("apply multi-file patch");
    assert_eq!(
        json_result(applied.result),
        json!({
            "changes": [
                { "kind": "updated", "path": "update.txt" },
                { "kind": "added", "path": "nested/new.txt" },
                { "kind": "deleted", "path": "delete.txt" },
                { "kind": "moved", "from": "move.txt", "to": "moved.txt" }
            ]
        })
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("update.txt")).expect("updated contents"),
        "alpha\nBETA\ngamma\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("nested/new.txt")).expect("added contents"),
        "new\n"
    );
    assert!(!root.path().join("delete.txt").exists());
    assert!(!root.path().join("move.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("moved.txt")).expect("moved contents"),
        "new\n"
    );

    std::fs::write(root.path().join("first.txt"), "before\n").expect("write first fixture");
    std::fs::write(root.path().join("second.txt"), "actual\n").expect("write second fixture");
    let invalid = r"*** Begin Patch
*** Update File: first.txt
@@
-before
+after
*** Update File: second.txt
@@
-missing
+replacement
*** End Patch";
    let source = format!(
        "lam.edit.apply({{ patch: {} }})",
        serde_json::to_string(invalid).expect("serialize invalid patch")
    );
    let failure = isolate
        .eval(&source)
        .await
        .expect_err("invalid later hunk must reject the whole plan");
    assert!(matches!(failure, EvalError::BuiltinFailure { .. }));
    assert_eq!(
        std::fs::read_to_string(root.path().join("first.txt")).expect("first unchanged"),
        "before\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn patch_handles_empty_files_and_deleting_the_only_line() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(root.path().join("empty.txt"), "").expect("write empty fixture");
    std::fs::write(root.path().join("single.txt"), "old\n").expect("write single fixture");
    std::fs::write(root.path().join("crlf.txt"), "one\r\ntwo\r\n").expect("write CRLF fixture");
    let pack = CodingPack::builder(root.path())
        .build()
        .expect("coding pack");
    let mut isolate = isolate(&pack).await;
    let patch = r"*** Begin Patch
*** Update File: empty.txt
@@
+hello
*** Update File: single.txt
@@
-old
*** Update File: crlf.txt
@@
-two
+TWO
*** End Patch";
    let source = format!(
        "await lam.edit.apply({{ patch: {} }})",
        serde_json::to_string(patch).expect("serialize patch")
    );

    isolate.eval(&source).await.expect("apply edge-case patch");
    assert_eq!(
        std::fs::read_to_string(root.path().join("empty.txt")).expect("updated empty file"),
        "hello"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("single.txt")).expect("updated single file"),
        ""
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("crlf.txt")).expect("updated CRLF file"),
        "one\r\nTWO\r\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn patch_rejects_parent_child_targets_before_mutation() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let pack = CodingPack::builder(root.path())
        .build()
        .expect("coding pack");
    let mut isolate = isolate(&pack).await;
    let patch = r"*** Begin Patch
*** Add File: conflict
+parent
*** Add File: conflict/child.txt
+child
*** End Patch";
    let source = format!(
        "lam.edit.apply({{ patch: {} }})",
        serde_json::to_string(patch).expect("serialize patch")
    );

    let failure = isolate
        .eval(&source)
        .await
        .expect_err("overlapping targets must fail during planning");
    assert!(matches!(failure, EvalError::BuiltinFailure { .. }));
    assert!(!root.path().join("conflict").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn write_creates_parents_and_complete_rewrites() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let pack = CodingPack::builder(root.path())
        .build()
        .expect("coding pack");
    let mut isolate = isolate(&pack).await;

    let created = isolate
        .eval("await lam.edit.write({ path: 'nested/file.txt', content: 'first\\n' })")
        .await
        .expect("create file");
    assert_eq!(json_result(created.result)["created"], true);
    let replaced = isolate
        .eval("await lam.edit.write({ path: 'nested/file.txt', content: 'second\\n' })")
        .await
        .expect("replace file");
    assert_eq!(json_result(replaced.result)["created"], false);
    assert_eq!(
        std::fs::read_to_string(root.path().join("nested/file.txt")).expect("written file"),
        "second\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn edit_accepts_line_arrays_so_backticks_do_not_break_eval_source() {
    let root = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(
        root.path().join("doc.rs"),
        "/// before\n/// uses lam.edit.apply.\n/// after\n",
    )
    .expect("write fixture");
    let pack = CodingPack::builder(root.path())
        .build()
        .expect("coding pack");
    let mut isolate = isolate(&pack).await;

    // Line arrays let models embed backticks without TypeScript template literals.
    // Double-quoted strings accept unescaped backticks, which is the whole point.
    let source = r#"
await lam.edit.apply({
  patch: [
    "*** Begin Patch",
    "*** Update File: doc.rs",
    "@@",
    " /// before",
    "-/// uses lam.edit.apply.",
    "+/// uses `lam.edit.apply`.",
    " /// after",
    "*** End Patch",
  ],
});
"#;
    let applied = isolate
        .eval(source)
        .await
        .expect("apply patch with backticks via line array");
    assert_eq!(
        json_result(applied.result),
        json!({ "changes": [{ "kind": "updated", "path": "doc.rs" }] })
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("doc.rs")).expect("patched file"),
        "/// before\n/// uses `lam.edit.apply`.\n/// after\n"
    );

    let written = isolate
        .eval(
            r##"await lam.edit.write({
  path: "notes.md",
  content: [
    "# Title",
    "See `lam.edit.apply` for patches.",
    "",
  ],
})"##,
        )
        .await
        .expect("write content with backticks via line array");
    assert_eq!(json_result(written.result)["created"], true);
    assert_eq!(
        std::fs::read_to_string(root.path().join("notes.md")).expect("written notes"),
        "# Title\nSee `lam.edit.apply` for patches.\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn shell_returns_normal_failures_and_spilled_output_is_numbered_by_fs_read() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let pack = CodingPack::builder(root.path())
        .shell_config(ShellConfig {
            default_timeout: Duration::from_secs(2),
            max_timeout: Duration::from_secs(3),
            capture: CaptureConfig {
                max_lines: 2,
                max_bytes: 64,
            },
        })
        .shell(LocalCommandRunner::default())
        .build()
        .expect("shell pack");
    let mut isolate = isolate(&pack).await;

    let result = isolate
        .eval(
            "const command = await lam.shell.run({ command: \"printf 'one\\ntwo\\nthree\\nfour\\n'; printf 'warning\\n' >&2; exit 7\" });\n\
             const full = await lam.fs.read({ path: command.stdout.fullOutputPath, limit: 10 });\n\
             lam.result({ command, full })",
        )
        .await
        .expect("nonzero shell outcome and spilled read");
    let result = json_result(result.result);
    assert_eq!(result["command"]["exitCode"], 7);
    assert_eq!(result["command"]["timedOut"], false);
    assert_eq!(result["command"]["stdout"]["content"], "three\nfour");
    assert_eq!(result["command"]["stdout"]["truncated"], true);
    assert_eq!(result["command"]["stderr"]["content"], "warning\n");
    assert_eq!(
        result["full"]["content"],
        "1\tone\n2\ttwo\n3\tthree\n4\tfour"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn shell_timeout_kills_background_descendants() {
    let root = tempfile::tempdir().expect("temporary workspace");
    let marker = root.path().join("orphaned.txt");
    let pack = CodingPack::builder(root.path())
        .shell_config(ShellConfig {
            default_timeout: Duration::from_secs(1),
            max_timeout: Duration::from_secs(2),
            capture: CaptureConfig::default(),
        })
        .shell(LocalCommandRunner::default())
        .build()
        .expect("shell pack");
    let mut isolate = isolate(&pack).await;
    let command = format!("(sleep 0.3; printf orphaned > '{}') &", marker.display());
    let source = format!(
        "await lam.shell.run({{ command: {}, timeoutMs: 50 }})",
        serde_json::to_string(&command).expect("serialize command")
    );
    let result = isolate
        .eval(&source)
        .await
        .expect("timeout is a normal outcome");
    assert_eq!(json_result(result.result)["timedOut"], true);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!marker.exists(), "background descendant survived timeout");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn filesystem_paths_reject_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("temporary workspace");
    let outside = tempfile::tempdir().expect("outside directory");
    let escaped = outside.path().join("escaped.txt");
    symlink(&escaped, root.path().join("link.txt")).expect("create broken symlink");
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "secret").expect("write outside fixture");
    symlink(&secret, root.path().join("secret-link.txt")).expect("create outside symlink");
    let pack = CodingPack::builder(root.path())
        .build()
        .expect("coding pack");
    let mut isolate = isolate(&pack).await;

    let failure = isolate
        .eval("lam.edit.write({ path: 'link.txt', content: 'escaped' })")
        .await
        .expect_err("broken symbolic link must be rejected");
    assert!(matches!(failure, EvalError::BuiltinFailure { .. }));
    assert!(!escaped.exists());
    let failure = isolate
        .eval("lam.fs.read({ path: 'secret-link.txt' })")
        .await
        .expect_err("reads through outside symbolic links must be rejected");
    assert!(matches!(failure, EvalError::BuiltinFailure { .. }));
}
