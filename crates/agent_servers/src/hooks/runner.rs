use std::{io, path::Path, pin::pin};

use futures::{
    AsyncWriteExt as _,
    future::{self, Either},
};
use gpui::BackgroundExecutor;
use serde_json::Value;
use util::command::{Stdio, new_command};

use super::HookCommand;

/// How a hook's process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookRun {
    /// The process exited; `code` is `None` when a signal ended it.
    Exited {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    /// The process outlived the hook's timeout and was killed.
    TimedOut,
    /// The process couldn't start, or its output couldn't be read.
    Failed(String),
}

/// A hook and how its run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookResult {
    pub command: HookCommand,
    pub run: HookRun,
}

/// Runs the hooks concurrently, each in `cwd` with `input` as JSON on stdin, and returns their
/// results in the order given.
pub async fn run_hooks(
    commands: Vec<HookCommand>,
    input: &Value,
    cwd: &Path,
    executor: &BackgroundExecutor,
) -> Vec<HookResult> {
    let input = input.to_string();
    future::join_all(commands.into_iter().map(|command| {
        let input = input.as_str();
        async move {
            let run = run_hook(&command, input, cwd, executor).await;
            HookResult { command, run }
        }
    }))
    .await
}

async fn run_hook(
    command: &HookCommand,
    input: &str,
    cwd: &Path,
    executor: &BackgroundExecutor,
) -> HookRun {
    let mut child = match new_command("/bin/sh")
        .arg("-c")
        .arg(&command.command)
        .current_dir(cwd)
        .env("CLAUDE_PROJECT_DIR", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return HookRun::Failed(error.to_string()),
    };

    let stdin = child.stdin.take();
    let write_input = async move {
        let Some(mut stdin) = stdin else {
            return;
        };
        // A hook may exit without reading its input.
        if let Err(error) = stdin.write_all(input.as_bytes()).await
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            log::warn!("writing hook input to {:?}: {error}", command.command);
        }
    };
    let finished = pin!(future::join(write_input, child.output()));

    // Dropping the unfinished output future drops the child, which kill_on_drop kills.
    match future::select(finished, executor.timer(command.timeout)).await {
        Either::Left(((_, Ok(output)), _)) => HookRun::Exited {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Either::Left(((_, Err(error)), _)) => HookRun::Failed(error.to_string()),
        Either::Right(_) => HookRun::TimedOut,
    }
}

// These run real processes through `/bin/sh`, so they need a Unix-like system.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn hook(command: &str, timeout: Duration) -> HookCommand {
        HookCommand {
            command: command.into(),
            timeout,
            blocks_on_failure: false,
        }
    }

    async fn run(command: &str, input: Value, cx: &mut TestAppContext) -> HookRun {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("hook directory");
        let mut results = run_hooks(
            vec![hook(command, Duration::from_secs(600))],
            &input,
            directory.path(),
            &cx.executor(),
        )
        .await;
        results.remove(0).run
    }

    fn exited(code: i32, stdout: &str, stderr: &str) -> HookRun {
        HookRun::Exited {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[gpui::test]
    async fn stdin_carries_the_input_json(cx: &mut TestAppContext) {
        let result = run("cat", json!({ "tool_name": "Bash" }), cx).await;
        assert_eq!(result, exited(0, r#"{"tool_name":"Bash"}"#, ""));
    }

    #[gpui::test]
    async fn exit_2_returns_stderr(cx: &mut TestAppContext) {
        let result = run("echo blocked >&2; exit 2", json!({}), cx).await;
        assert_eq!(result, exited(2, "", "blocked\n"));
    }

    #[gpui::test]
    async fn missing_command_exits_127(cx: &mut TestAppContext) {
        let result = run("/nonexistent/hook", json!({}), cx).await;
        assert!(
            matches!(
                result,
                HookRun::Exited {
                    code: Some(127),
                    ..
                }
            ),
            "{result:?}"
        );
    }

    #[gpui::test]
    async fn a_hook_that_ignores_large_input_still_finishes(cx: &mut TestAppContext) {
        let input = json!({ "prompt": "x".repeat(1 << 20) });
        let result = run("exit 0", input, cx).await;
        assert_eq!(result, exited(0, "", ""));
    }

    #[gpui::test]
    async fn runs_in_cwd_with_claude_project_dir(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("hook directory");
        let results = run_hooks(
            vec![hook(
                r#"pwd -P; printf %s "$CLAUDE_PROJECT_DIR""#,
                Duration::from_secs(600),
            )],
            &json!({}),
            directory.path(),
            &cx.executor(),
        )
        .await;
        let real_path = directory.path().canonicalize().expect("real path");
        let expected = format!("{}\n{}", real_path.display(), directory.path().display());
        assert_eq!(results[0].run, exited(0, &expected, ""));
    }

    #[gpui::test]
    async fn hooks_run_concurrently_and_keep_their_order(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("hook directory");
        let started = Instant::now();
        let results = run_hooks(
            vec![
                hook("sleep 1; echo first", Duration::from_secs(600)),
                hook("sleep 1; echo second", Duration::from_secs(600)),
            ],
            &json!({}),
            directory.path(),
            &cx.executor(),
        )
        .await;
        assert!(started.elapsed() < Duration::from_millis(1900));
        assert_eq!(results[0].run, exited(0, "first\n", ""));
        assert_eq!(results[1].run, exited(0, "second\n", ""));
    }

    #[gpui::test]
    async fn a_hanging_hook_times_out_without_waiting_for_it(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("hook directory");
        let executor = cx.executor();
        let started = Instant::now();
        let task = executor.spawn({
            let path = directory.path().to_path_buf();
            let executor = executor.clone();
            async move {
                run_hooks(
                    vec![hook("sleep 30", Duration::from_secs(1))],
                    &json!({}),
                    &path,
                    &executor,
                )
                .await
            }
        });
        // Runs the hook until it waits, then moves the test clock past its timeout.
        executor.run_until_parked();
        assert_eq!(task.await[0].run, HookRun::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
