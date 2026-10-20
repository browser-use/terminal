//! Fail-closed command inspection, independent of approval policy and its cache.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::hooks::{HookDecision, HookInput};

use super::runtime::ToolError;

pub(super) fn from_env() -> Option<Arc<Vec<String>>> {
    std::env::var_os("BUT_COMMAND_GUARD").map(|raw| {
        // Invalid configuration stays enabled and fails closed at execution time.
        let command: Vec<String> = raw
            .to_str()
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_default();
        if command.first().is_none_or(String::is_empty) {
            eprintln!("warning: invalid BUT_COMMAND_GUARD; expected a nonempty JSON argv array; commands will be denied");
        }
        Arc::new(command)
    })
}

pub(super) async fn inspect(command: &[String], input: HookInput) -> Result<(), ToolError> {
    let denied = |reason: &str| {
        eprintln!("warning: command guard: {reason}");
        ToolError::Rejected("Command blocked by pre-execution guard.".into())
    };
    let program = command
        .first()
        .filter(|program| !program.is_empty())
        .ok_or_else(|| denied("invalid checker configuration"))?;
    let payload = serde_json::to_vec(&input).map_err(|_| denied("cannot encode checker input"))?;
    let mut child = Command::new(program)
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Checkers can echo sensitive command text; only report fixed failure categories.
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| denied("cannot start checker"))?;

    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| denied("checker stdin unavailable"))?;
        stdin
            .write_all(&payload)
            .await
            .map_err(|_| denied("cannot write checker input"))?;
        drop(stdin);

        let mut output = Vec::new();
        child
            .stdout
            .take()
            .ok_or_else(|| denied("checker stdout unavailable"))?
            .take(8 * 1024 + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|_| denied("cannot read checker output"))?;
        if output.len() > 8 * 1024 {
            return Err(denied("checker output exceeds 8 KiB"));
        }
        if !child
            .wait()
            .await
            .map_err(|_| denied("cannot wait for checker"))?
            .success()
        {
            return Err(denied("checker exited unsuccessfully"));
        }
        let decision: HookDecision =
            serde_json::from_slice(&output).map_err(|_| denied("invalid checker response"))?;
        if decision.r#continue != Some(true) {
            return Err(denied("checker did not explicitly allow execution"));
        }
        Ok(())
    })
    .await
    .map_err(|_| denied("checker timed out"))?
}

#[cfg(all(test, unix))]
mod tests {
    use super::inspect;
    use crate::hooks::{HookEvent, HookInput};

    #[tokio::test]
    async fn command_guard_requires_a_successful_explicit_allow_decision() {
        for script in [
            "cat >/dev/null; printf '{\"continue\":true}'; exit 1",
            "cat >/dev/null; printf '{}'",
            "cat >/dev/null; printf '{\"continue\":\"true\"}'",
            "cat >/dev/null; printf 'not json'",
        ] {
            let result = inspect(
                &["sh".into(), "-c".into(), script.into()],
                HookInput::new(HookEvent::PreToolUse),
            )
            .await;
            assert!(result.is_err(), "checker must fail closed: {script}");
        }
        for command in [
            vec![],
            vec!["/nonexistent/browser-use-command-guard".into()],
        ] {
            assert!(inspect(&command, HookInput::new(HookEvent::PreToolUse))
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn command_guard_enforces_the_output_byte_limit_on_valid_json() {
        for size in [8 * 1024, 8 * 1024 + 1] {
            let mut output = String::from("{\"continue\":true}");
            output.push_str(&" ".repeat(size - output.len()));
            assert!(serde_json::from_str::<crate::hooks::HookDecision>(&output).is_ok());
            let result = inspect(
                &[
                    "sh".into(),
                    "-c".into(),
                    "cat >/dev/null; printf '%s' \"$1\"".into(),
                    "guard".into(),
                    output,
                ],
                HookInput::new(HookEvent::PreToolUse),
            )
            .await;
            assert_eq!(result.is_ok(), size == 8 * 1024);
        }
    }

    #[tokio::test]
    async fn command_guard_times_out_without_allowing_execution() {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(7),
            inspect(
                &[
                    "sh".into(),
                    "-c".into(),
                    "cat >/dev/null; exec sleep 30".into(),
                ],
                HookInput::new(HookEvent::PreToolUse),
            ),
        )
        .await
        .expect("checker must honor the five-second timeout");
        assert!(result.is_err());
    }
}
