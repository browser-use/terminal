//! Tests for the async shell/exec tool ([`ShellTool`]).
//!
//! All tests use local process execution (no network). They drive the tool
//! directly through the [`ToolRuntime::run`] seam (with a `SandboxType::None`
//! attempt) and through the [`ToolOrchestrator`] for the approval/denylist path.

use std::collections::HashMap;

use super::shell::{
    dangerous_command_rejection, ExecCommandRequest, ExecCommandTool, ShellRequest, ShellTool,
    WriteStdinRequest, WriteStdinTool, DEFAULT_SHELL_COMMAND_TIMEOUT_MS, MAX_STREAM_OUTPUT_BYTES,
    TIMEOUT_EXIT_CODE,
};
use crate::tools::approval::AskForApproval;
use crate::tools::orchestrator::{ToolOrchestrator, TurnEnv};
use crate::tools::runtime::{
    Approvable, AutoApprover, ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{
    FileSystemSandboxPolicy, NoneSandboxProvider, SandboxLaunch, SandboxPermissions, SandboxType,
};
use crate::tools::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS;
use crate::tools::UnifiedExecManager;

/// A `SandboxType::None` launch + attempt for direct `run` calls.
fn none_launch() -> SandboxLaunch {
    SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: None,
    }
}

fn none_attempt(launch: &SandboxLaunch) -> SandboxAttempt<'_> {
    SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch,
        cancel: None,
    }
}

/// A minimal ctx rooted at a given cwd.
fn ctx_in(cwd: &std::path::Path) -> ToolCtx {
    ToolCtx {
        call_id: "test-call".to_string(),
        tool_name: "shell".to_string(),
        cwd: cwd.to_path_buf(),
        artifact_root: cwd.join("artifacts"),
    }
}

fn ctx_for(cwd: &std::path::Path, tool_name: &str) -> ToolCtx {
    ToolCtx {
        call_id: format!("{tool_name}-call"),
        tool_name: tool_name.to_string(),
        cwd: cwd.to_path_buf(),
        artifact_root: cwd.join("artifacts"),
    }
}

/// Run a shell request directly through the runtime (no orchestrator).
async fn run_direct(req: &ShellRequest, ctx: &ToolCtx) -> Result<ExecOutput, ToolError> {
    let tool = ShellTool::new();
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    tool.run(req, &attempt, ctx).await
}

fn session_id_from_model_text(text: &str) -> i32 {
    text.lines()
        .find_map(|line| line.strip_prefix("Process running with session ID "))
        .expect("missing running session id")
        .parse()
        .expect("session id parses")
}

#[tokio::test]
async fn exec_command_returns_session_id_and_write_stdin_polls_to_exit() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::deterministic_for_tests();
    let exec = ExecCommandTool::new(manager.clone());
    let write = WriteStdinTool::new(manager);
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let exec_ctx = ctx_for(dir.path(), "exec_command");
    let write_ctx = ctx_for(dir.path(), "write_stdin");

    let first = exec
        .run(
            &ExecCommandRequest {
                cmd: Some("printf start; sleep 1; printf done".to_string()),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(10),
                timeout_ms: None,
                max_output_tokens: None,
                env: HashMap::new(),
                tty: false,
            },
            &attempt,
            &exec_ctx,
        )
        .await
        .expect("exec_command starts");
    assert_eq!(first.exit_code, 0);
    let session_id = session_id_from_model_text(&first.stdout);
    assert!(first.stdout.contains("Chunk ID:"));
    assert!(first.stdout.contains("Wall time:"));
    assert!(first.stdout.contains("Original token count:"));
    assert!(first.stdout.contains("Output:\nstart"));

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let second = write
        .run(
            &WriteStdinRequest {
                session_id,
                chars: String::new(),
                yield_time_ms: Some(50),
                max_output_tokens: None,
            },
            &attempt,
            &write_ctx,
        )
        .await
        .expect("write_stdin polls");
    assert!(second.stdout.contains("Process exited with code 0"));
    assert!(second.stdout.contains("Output:\ndone"));
}

#[tokio::test]
async fn exec_command_timeout_kills_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::deterministic_for_tests();
    let exec = ExecCommandTool::new(manager);
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let exec_ctx = ctx_for(dir.path(), "exec_command");
    let marker = dir.path().join("late.txt");

    let out = exec
        .run(
            &ExecCommandRequest {
                cmd: Some("(sleep 1; printf bad > late.txt) & sleep 5".to_string()),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(1_000),
                timeout_ms: Some(100),
                max_output_tokens: None,
                env: HashMap::new(),
                tty: false,
            },
            &attempt,
            &exec_ctx,
        )
        .await
        .expect("exec_command returns timeout snapshot");

    assert!(out.stdout.contains("Process exited with code 124"));
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    assert!(
        !marker.exists(),
        "exec_command timeout should kill background children"
    );
}

#[tokio::test]
async fn write_stdin_rejects_non_tty_input() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::deterministic_for_tests();
    let exec = ExecCommandTool::new(manager.clone());
    let write = WriteStdinTool::new(manager);
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let exec_ctx = ctx_for(dir.path(), "exec_command");
    let write_ctx = ctx_for(dir.path(), "write_stdin");

    let first = exec
        .run(
            &ExecCommandRequest {
                cmd: Some("sleep 1".to_string()),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(10),
                timeout_ms: None,
                max_output_tokens: None,
                env: HashMap::new(),
                tty: false,
            },
            &attempt,
            &exec_ctx,
        )
        .await
        .expect("exec_command starts");
    let session_id = session_id_from_model_text(&first.stdout);

    let err = write
        .run(
            &WriteStdinRequest {
                session_id,
                chars: "hello\n".to_string(),
                yield_time_ms: Some(250),
                max_output_tokens: None,
            },
            &attempt,
            &write_ctx,
        )
        .await
        .expect_err("non-tty stdin should reject");
    match err {
        ToolError::Other(err) => assert!(
            err.to_string().contains("stdin is closed"),
            "unexpected error: {err}"
        ),
        other => panic!("expected stdin closed error, got {other:?}"),
    }
}

#[tokio::test]
async fn tty_write_stdin_sends_input_and_returns_exit_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::default();
    let exec = ExecCommandTool::new(manager.clone());
    let write = WriteStdinTool::new(manager);
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let exec_ctx = ctx_for(dir.path(), "exec_command");
    let write_ctx = ctx_for(dir.path(), "write_stdin");

    let first = exec
        .run(
            &ExecCommandRequest {
                cmd: Some("read line; echo got:$line".to_string()),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(10),
                timeout_ms: None,
                max_output_tokens: None,
                env: HashMap::new(),
                tty: true,
            },
            &attempt,
            &exec_ctx,
        )
        .await
        .expect("pty exec starts");
    let session_id = session_id_from_model_text(&first.stdout);

    let second = write
        .run(
            &WriteStdinRequest {
                session_id,
                chars: "hello unified exec\n".to_string(),
                yield_time_ms: Some(1_000),
                max_output_tokens: None,
            },
            &attempt,
            &write_ctx,
        )
        .await
        .expect("pty stdin writes");
    assert!(second.stdout.contains("Process exited with code 0"));
    assert!(
        second.stdout.contains("got:hello unified exec"),
        "output should include command response, got: {}",
        second.stdout
    );
}

#[tokio::test]
async fn exec_command_applies_codex_env_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::deterministic_for_tests();
    let exec = ExecCommandTool::new(manager);
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let exec_ctx = ctx_for(dir.path(), "exec_command");
    let mut env = HashMap::new();
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("PAGER".to_string(), "less".to_string());

    let out = exec
        .run(
            &ExecCommandRequest {
                cmd: Some(
                    "printf '%s|%s|%s|%s' \"$NO_COLOR\" \"$TERM\" \"$PAGER\" \"$CODEX_CI\""
                        .to_string(),
                ),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(1_000),
                timeout_ms: None,
                max_output_tokens: None,
                env,
                tty: false,
            },
            &attempt,
            &exec_ctx,
        )
        .await
        .expect("exec completes");

    assert!(
        out.stdout.contains("1|dumb|cat|1"),
        "Codex env defaults should override request/env noise: {}",
        out.stdout
    );
}

#[tokio::test]
async fn exec_command_interruption_preserves_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::deterministic_for_tests();
    let exec = ExecCommandTool::new(manager.clone());
    let write = WriteStdinTool::new(manager);
    let exec_ctx = ctx_for(dir.path(), "exec_command");
    let write_ctx = ctx_for(dir.path(), "write_stdin");

    let handle = tokio::spawn(async move {
        let launch = none_launch();
        let attempt = none_attempt(&launch);
        exec.run(
            &ExecCommandRequest {
                cmd: Some("printf ready; sleep 1; printf survived".to_string()),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(5_000),
                timeout_ms: None,
                max_output_tokens: None,
                env: HashMap::new(),
                tty: false,
            },
            &attempt,
            &exec_ctx,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    handle.abort();
    let _ = handle.await;

    tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    let poll_launch = none_launch();
    let poll_attempt = none_attempt(&poll_launch);
    let polled = write
        .run(
            &WriteStdinRequest {
                session_id: 1000,
                chars: String::new(),
                yield_time_ms: Some(250),
                max_output_tokens: None,
            },
            &poll_attempt,
            &write_ctx,
        )
        .await
        .expect("aborted exec future leaves live session pollable");
    assert!(polled.stdout.contains("Process exited with code 0"));
    assert!(polled.stdout.contains("survived"), "got: {}", polled.stdout);
}

#[tokio::test]
async fn exec_command_cancel_returns_live_session_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::deterministic_for_tests();
    let exec = ExecCommandTool::new(manager.clone());
    let write = WriteStdinTool::new(manager);
    let exec_ctx = ctx_for(dir.path(), "exec_command");
    let write_ctx = ctx_for(dir.path(), "write_stdin");
    let cancel = tokio_util::sync::CancellationToken::new();
    let launch = SandboxLaunch {
        sandbox: SandboxType::None,
        cancel: Some(cancel.clone()),
    };
    let attempt = SandboxAttempt {
        sandbox: SandboxType::None,
        permissions: SandboxPermissions::UseDefault,
        enforce_managed_network: false,
        launch: &launch,
        cancel: Some(cancel.clone()),
    };

    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel2.cancel();
    });

    let started = std::time::Instant::now();
    let first = exec
        .run(
            &ExecCommandRequest {
                cmd: Some("printf ready; sleep 1; printf survived".to_string()),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(30_000),
                timeout_ms: None,
                max_output_tokens: None,
                env: HashMap::new(),
                tty: false,
            },
            &attempt,
            &exec_ctx,
        )
        .await
        .expect("exec returns snapshot on cancel");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "exec should return when cancelled instead of waiting full yield"
    );
    assert!(first
        .stdout
        .contains("Process running with session ID 1000"));
    assert!(first.stdout.contains("ready"));

    tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    let poll_launch = none_launch();
    let poll_attempt = none_attempt(&poll_launch);
    let polled = write
        .run(
            &WriteStdinRequest {
                session_id: 1000,
                chars: String::new(),
                yield_time_ms: Some(250),
                max_output_tokens: None,
            },
            &poll_attempt,
            &write_ctx,
        )
        .await
        .expect("cancelled exec remains pollable");
    assert!(polled.stdout.contains("Process exited with code 0"));
    assert!(polled.stdout.contains("survived"), "got: {}", polled.stdout);
}

#[tokio::test]
async fn terminate_all_best_effort_kills_managed_processes() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::deterministic_for_tests();
    let exec = ExecCommandTool::new(manager.clone());
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let exec_ctx = ctx_for(dir.path(), "exec_command");
    let marker = dir.path().join("survived");

    let first = exec
        .run(
            &ExecCommandRequest {
                cmd: Some("sleep 1; touch survived".to_string()),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(250),
                timeout_ms: None,
                max_output_tokens: None,
                env: HashMap::new(),
                tty: false,
            },
            &attempt,
            &exec_ctx,
        )
        .await
        .expect("exec starts");
    assert!(first
        .stdout
        .contains("Process running with session ID 1000"));

    assert_eq!(manager.terminate_all_best_effort(), 1);
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    assert!(
        !marker.exists(),
        "cleanup should kill the managed command before it can write {marker:?}"
    );
}

#[tokio::test]
async fn exec_command_max_output_tokens_truncates_model_output() {
    let dir = tempfile::tempdir().unwrap();
    let manager = UnifiedExecManager::default();
    let exec = ExecCommandTool::new(manager);
    let launch = none_launch();
    let attempt = none_attempt(&launch);
    let exec_ctx = ctx_for(dir.path(), "exec_command");

    let out = exec
        .run(
            &ExecCommandRequest {
                cmd: Some("printf abcdefghijklmnopqrstuvwxyz".to_string()),
                command: None,
                workdir: None,
                cwd: None,
                shell: None,
                login: None,
                yield_time_ms: Some(1_000),
                timeout_ms: None,
                max_output_tokens: Some(2),
                env: HashMap::new(),
                tty: false,
            },
            &attempt,
            &exec_ctx,
        )
        .await
        .expect("exec completes");
    assert!(out.stdout.contains("\n…\n"), "got: {}", out.stdout);
    assert!(
        !out.stdout.contains("abcdefghijklmnopqrstuvwxyz"),
        "full output should be truncated: {}",
        out.stdout
    );
}

#[tokio::test]
async fn shell_cancellation_kills_running_child() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let tool = ShellTool::new();
    let cancel = tokio_util::sync::CancellationToken::new();
    let req = ShellRequest {
        command: vec!["sh".to_string(), "-c".to_string(), "sleep 5".to_string()],
        cwd: None,
        timeout_ms: Some(10_000),
        env: HashMap::new(),
        max_output_tokens: None,
    };

    let run_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        let launch = SandboxLaunch {
            sandbox: SandboxType::None,
            cancel: Some(run_cancel.clone()),
        };
        let attempt = SandboxAttempt {
            sandbox: SandboxType::None,
            permissions: SandboxPermissions::UseDefault,
            enforce_managed_network: false,
            launch: &launch,
            cancel: Some(run_cancel),
        };
        tool.run(&req, &attempt, &ctx).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let out = handle.await.unwrap().expect("cancel returns output");
    assert_eq!(out.exit_code, 130);
}

// (1) `echo hello` -> exit 0, stdout contains "hello".
#[tokio::test]
async fn echo_hello_exit_zero_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["echo", "hello"]);

    let out = run_direct(&req, &ctx).await.expect("echo should succeed");
    assert_eq!(out.exit_code, 0, "echo should exit 0");
    assert!(
        out.stdout.contains("hello"),
        "stdout should contain 'hello', got: {:?}",
        out.stdout
    );
    assert!(out.stderr.is_empty(), "stderr should be empty");
}

// (2) A command that exits non-zero -> captured exit code.
#[tokio::test]
async fn nonzero_exit_code_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["sh", "-c", "exit 7"]);

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert_eq!(out.exit_code, 7, "exit code 7 should be captured");
}

// (3) A command that sleeps longer than a short timeout -> times out.
//     Per our design (and codex exec.rs:746-748), this surfaces as an
//     `ExecOutput` carrying exit code 124.
#[tokio::test]
async fn long_command_times_out_with_124() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let mut req = ShellRequest::from_argv(["sh", "-c", "sleep 5"]);
    req.timeout_ms = Some(150); // much shorter than the 5s sleep

    let out = run_direct(&req, &ctx)
        .await
        .expect("timeout yields ExecOutput");
    assert_eq!(
        out.exit_code, TIMEOUT_EXIT_CODE,
        "timeout must report exit code 124"
    );
    assert_eq!(
        TIMEOUT_EXIT_CODE, 124,
        "parity: codex EXEC_TIMEOUT_EXIT_CODE"
    );
    assert!(
        out.stderr.contains("timed out"),
        "stderr should note the timeout, got: {:?}",
        out.stderr
    );
}

// (4) Output exceeding the byte cap -> truncated with a marker.
#[tokio::test]
async fn oversized_output_is_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let want = MAX_STREAM_OUTPUT_BYTES + 4096;
    // Deterministic, bounded payload: N bytes of 'a'.
    let script = format!("head -c {want} /dev/zero | tr '\\0' 'a'");
    let mut req = ShellRequest::from_argv(["sh", "-c", &script]);
    req.max_output_tokens = Some(MAX_STREAM_OUTPUT_BYTES);

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("[... omitted"),
        "expected truncation marker, stdout len = {}",
        out.stdout.len()
    );
    // Strip the truncation marker before measuring the retained payload (the
    // marker text itself contains a couple of 'a's).
    let a_count = out.stdout.matches('a').count();
    assert_eq!(
        a_count, MAX_STREAM_OUTPUT_BYTES,
        "retained payload should be exactly the byte cap (cap honored, no overflow)"
    );
}

#[tokio::test]
async fn shell_default_model_output_tokens_truncates_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let want = DEFAULT_MAX_OUTPUT_TOKENS * 4 + 4096;
    let script = format!("head -c {want} /dev/zero | tr '\\0' 'a'");
    let req = ShellRequest::from_argv(["sh", "-c", &script]);

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.contains("\n…\n"),
        "expected model-output truncation marker, stdout len = {}",
        out.stdout.len()
    );
    assert_eq!(
        out.stdout.matches('a').count(),
        DEFAULT_MAX_OUTPUT_TOKENS * 4,
        "default shell output should be capped before it is re-fed to the model"
    );
}

// (5) cwd is respected: run `pwd` in a tempdir and assert it echoes that dir.
//     Uses the ctx cwd (request cwd = None) to exercise the ctx fallback.
#[tokio::test]
async fn ctx_cwd_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    let canon = std::fs::canonicalize(dir.path()).unwrap();
    let ctx = ctx_in(&canon);
    let req = ShellRequest::from_argv(["pwd"]);

    let out = run_direct(&req, &ctx).await.expect("pwd should run");
    assert_eq!(out.exit_code, 0);
    let reported = std::fs::canonicalize(out.stdout.trim()).unwrap();
    assert_eq!(reported, canon, "pwd should report the ctx tempdir");
}

// (5b) cwd from the request itself overrides the ctx cwd.
#[tokio::test]
async fn request_cwd_overrides_ctx_cwd() {
    let ctx_dir = tempfile::tempdir().unwrap();
    let req_dir = tempfile::tempdir().unwrap();
    let req_canon = std::fs::canonicalize(req_dir.path()).unwrap();
    let ctx = ctx_in(ctx_dir.path());

    let mut req = ShellRequest::from_argv(["pwd"]);
    req.cwd = Some(req_canon.clone());

    let out = run_direct(&req, &ctx).await.expect("pwd should run");
    let reported = std::fs::canonicalize(out.stdout.trim()).unwrap();
    assert_eq!(reported, req_canon, "request cwd should win over ctx cwd");
}

// Env vars from the request are passed to the child.
#[tokio::test]
async fn env_is_passed_to_child() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let mut env = HashMap::new();
    env.insert("WP_T_SHELL_VAR".to_string(), "marker-123".to_string());
    let mut req = ShellRequest::from_argv(["sh", "-c", "echo $WP_T_SHELL_VAR"]);
    req.env = env;

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert!(
        out.stdout.contains("marker-123"),
        "env var should be visible, got: {:?}",
        out.stdout
    );
}

// stderr is captured separately from stdout.
#[tokio::test]
async fn stderr_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["sh", "-c", "echo oops 1>&2; exit 3"]);

    let out = run_direct(&req, &ctx).await.expect("command should run");
    assert_eq!(out.exit_code, 3);
    assert!(out.stdout.is_empty(), "stdout should be empty");
    assert!(
        out.stderr.contains("oops"),
        "stderr should contain 'oops', got: {:?}",
        out.stderr
    );
}

// Empty command is rejected (mapped to ToolError::Other).
#[tokio::test]
async fn empty_command_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest {
        command: vec![],
        cwd: None,
        timeout_ms: None,
        env: HashMap::new(),
        max_output_tokens: None,
    };
    match run_direct(&req, &ctx).await {
        Err(ToolError::Other(_)) => {}
        other => panic!("expected Other for empty command, got {other:?}"),
    }
}

// Default timeout falls back to the codex/legacy 10s constant.
#[test]
fn default_timeout_constant() {
    assert_eq!(
        DEFAULT_SHELL_COMMAND_TIMEOUT_MS, 10_000,
        "default timeout must match codex/legacy 10s"
    );
}

// Denylist: a destructive `rm -rf /` is rejected at `run` (defense in depth).
#[tokio::test]
async fn destructive_command_rejected_at_run() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["rm", "-rf", "/"]);
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(msg)) => {
            assert!(
                msg.contains("denylist"),
                "rejection should mention denylist, got: {msg}"
            );
        }
        other => panic!("expected destructive command to be rejected, got {other:?}"),
    }
}

// Denylist: catches the `sh -c "rm -rf ~"` payload form too.
#[tokio::test]
async fn destructive_command_in_sh_c_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    let req = ShellRequest::from_argv(["sh", "-c", "rm -rf ~"]);
    match run_direct(&req, &ctx).await {
        Err(ToolError::Rejected(_)) => {}
        other => panic!("expected rejection for `sh -c rm -rf ~`, got {other:?}"),
    }
}

// Denylist: `sudo rm -rf /` (sudo-wrapped) is rejected.
#[tokio::test]
async fn sudo_destructive_command_rejected() {
    let req = ShellRequest::from_argv(["sudo", "rm", "-rf", "/"]);
    assert!(
        dangerous_command_rejection(&req.command).is_err(),
        "sudo-wrapped rm -rf / must be rejected"
    );
}

// Denylist: a benign scoped `rm -rf ./build` is NOT rejected (no false positive).
#[tokio::test]
async fn scoped_rm_is_not_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    std::fs::create_dir(dir.path().join("build")).unwrap();
    let req = ShellRequest::from_argv(["rm", "-rf", "build"]);
    let out = run_direct(&req, &ctx)
        .await
        .expect("scoped rm should be allowed");
    assert_eq!(out.exit_code, 0);
    assert!(!dir.path().join("build").exists());
}

// The tool is not parallel-safe (serial / write-lock, matching codex shell).
#[test]
fn shell_is_not_parallel_safe() {
    let tool = ShellTool::new();
    let req = ShellRequest::from_argv(["echo", "x"]);
    assert!(!tool.parallel_safe(&req));
}

// Approval/sandbox accessors: a destructive command yields a Forbidden
// requirement; a benign one defers to the policy default (None).
#[test]
fn exec_approval_requirement_forbids_destructive_only() {
    use crate::tools::approval::ExecApprovalRequirement;
    let tool = ShellTool::new();
    let danger = ShellRequest::from_argv(["rm", "-rf", "/"]);
    let benign = ShellRequest::from_argv(["echo", "hi"]);

    assert!(
        matches!(
            tool.exec_approval_requirement(&danger),
            Some(ExecApprovalRequirement::Forbidden { .. })
        ),
        "destructive command must be forbidden"
    );
    assert!(
        tool.exec_approval_requirement(&benign).is_none(),
        "benign command defers to the policy default"
    );
    assert_eq!(
        tool.approval_keys(&benign).len(),
        1,
        "one approval key per shell call"
    );
    assert_eq!(
        tool.sandbox_permissions(&benign),
        SandboxPermissions::UseDefault
    );
}

// --- Orchestrator integration: drive the shell tool through the full seam. ---

fn turn_env(restricted: bool) -> TurnEnv {
    TurnEnv {
        file_system_sandbox_policy: FileSystemSandboxPolicy {
            restricted,
            denied_read: false,
        },
        managed_network_active: false,
        strict_auto_review: false,
        use_guardian: false,
    }
}

#[tokio::test]
async fn orchestrated_echo_completes_under_none() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    // `Never` => no approval prompt for a benign command.
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = ShellTool::new();
    let req = ShellRequest::from_argv(["echo", "orchestrated"]);

    let result = orch
        .run(&tool, &req, &ctx, &turn_env(false), AskForApproval::Never)
        .await
        .expect("orchestration ok");

    assert_eq!(result.sandbox_used, SandboxType::None);
    assert_eq!(result.output.exit_code, 0);
    assert!(result.output.stdout.contains("orchestrated"));
}

#[tokio::test]
async fn orchestrated_destructive_is_rejected_by_defense_in_depth() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ctx_in(dir.path());
    // Forbidden requirement => needs_initial_approval; AutoApprover approves,
    // but `run`'s defense-in-depth denylist still rejects the command.
    let orch = ToolOrchestrator::new(NoneSandboxProvider, AutoApprover);
    let tool = ShellTool::new();
    let req = ShellRequest::from_argv(["rm", "-rf", "/"]);

    let err = orch
        .run(&tool, &req, &ctx, &turn_env(false), AskForApproval::Never)
        .await
        .expect_err("destructive command must not complete");
    assert!(
        matches!(err, ToolError::Rejected(_)),
        "expected Rejected, got {err:?}"
    );
}
