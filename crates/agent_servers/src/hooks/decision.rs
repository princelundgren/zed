use serde_json::{Map, Value};

use super::runner::{HookResult, HookRun};

/// What `PreToolUse` hooks decided about a tool call; a deny beats an ask, which beats an allow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Ask,
    Deny {
        reason: String,
    },
    /// No hook allowed, asked or denied.
    Undecided,
}

/// What `UserPromptSubmit` hooks decided about a prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptOutcome {
    /// Send the prompt, with the hooks' context ahead of it.
    Continue {
        context: Vec<String>,
    },
    Block {
        reason: String,
    },
}

/// Whether `Stop` hooks let the agent end its turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    Stop,
    /// Keep going: `reason` is the agent's next prompt.
    Continue {
        reason: String,
    },
}

/// A hook's run, read by Claude Code's exit-code and stdout rules.
enum HookOutput {
    /// Exit 0 with one JSON object on stdout.
    Json(Map<String, Value>),
    /// Exit 0 with anything else on stdout, trimmed.
    Text(String),
    /// Exit 2, with stderr as the reason.
    Block(String),
    /// Any other exit, invalid JSON, a timeout or a failure to run.
    Failed { message: String, blocks: bool },
}

fn read_output(result: &HookResult) -> HookOutput {
    let command = &result.command.command;
    let failed = |problem: String| {
        let message = format!("hook {command:?} {problem}");
        log::warn!("{message}");
        HookOutput::Failed {
            message,
            blocks: result.command.blocks_on_failure,
        }
    };
    match &result.run {
        HookRun::Exited {
            code: Some(0),
            stdout,
            ..
        } => {
            let stdout = stdout.trim();
            if stdout.starts_with('{') && stdout.ends_with('}') {
                match serde_json::from_str(stdout) {
                    Ok(object) => HookOutput::Json(object),
                    Err(error) => failed(format!("printed invalid JSON: {error}")),
                }
            } else {
                HookOutput::Text(stdout.to_string())
            }
        }
        HookRun::Exited {
            code: Some(2),
            stderr,
            ..
        } => match stderr.trim() {
            "" => HookOutput::Block(format!("hook {command:?} exited with code 2")),
            reason => HookOutput::Block(reason.to_string()),
        },
        HookRun::Exited {
            code: Some(code),
            stderr,
            ..
        } => failed(format!("exited with code {code}: {}", stderr.trim())),
        HookRun::Exited { code: None, .. } => failed("was ended by a signal".into()),
        HookRun::TimedOut => failed(format!(
            "timed out after {}s",
            result.command.timeout.as_secs()
        )),
        HookRun::Failed(error) => failed(format!("failed to run: {error}")),
    }
}

fn string_field<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    object.get(field).and_then(Value::as_str)
}

fn hook_specific_output(object: &Map<String, Value>) -> Option<&Map<String, Value>> {
    object.get("hookSpecificOutput").and_then(Value::as_object)
}

fn additional_context(object: &Map<String, Value>) -> Option<String> {
    hook_specific_output(object)
        .and_then(|output| string_field(output, "additionalContext"))
        .map(str::to_string)
}

/// The decision field `block` with its `reason`, or a default naming the hook.
fn block_reason(object: &Map<String, Value>, command: &str) -> Option<String> {
    (string_field(object, "decision") == Some("block")).then(|| {
        string_field(object, "reason")
            .map_or_else(|| format!("hook {command:?} blocked"), str::to_string)
    })
}

/// Reads `PreToolUse` results.
pub fn tool_decision(results: &[HookResult]) -> ToolDecision {
    let mut denials = Vec::new();
    let mut asked = false;
    let mut allowed = false;
    for result in results {
        let object = match read_output(result) {
            HookOutput::Block(reason)
            | HookOutput::Failed {
                message: reason,
                blocks: true,
            } => {
                denials.push(reason);
                continue;
            }
            HookOutput::Failed { blocks: false, .. } | HookOutput::Text(_) => continue,
            HookOutput::Json(object) => object,
        };
        let specific = hook_specific_output(&object);
        let decision = specific
            .and_then(|output| string_field(output, "permissionDecision"))
            .or_else(|| match string_field(&object, "decision") {
                Some("approve") => Some("allow"),
                Some("block") => Some("deny"),
                _ => None,
            });
        match decision {
            Some("deny") => denials.push(
                specific
                    .and_then(|output| string_field(output, "permissionDecisionReason"))
                    .or_else(|| string_field(&object, "reason"))
                    .map_or_else(
                        || format!("hook {:?} denied the tool call", result.command.command),
                        str::to_string,
                    ),
            ),
            Some("ask") => asked = true,
            Some("allow") => allowed = true,
            _ => {}
        }
    }
    if !denials.is_empty() {
        ToolDecision::Deny {
            reason: denials.join("\n"),
        }
    } else if asked {
        ToolDecision::Ask
    } else if allowed {
        ToolDecision::Allow
    } else {
        ToolDecision::Undecided
    }
}

/// Reads `UserPromptSubmit` results.
pub fn prompt_outcome(results: &[HookResult]) -> PromptOutcome {
    let mut context = Vec::new();
    let mut blocks = Vec::new();
    for result in results {
        match read_output(result) {
            HookOutput::Block(reason)
            | HookOutput::Failed {
                message: reason,
                blocks: true,
            } => blocks.push(reason),
            HookOutput::Failed { blocks: false, .. } => {}
            HookOutput::Text(text) => context.extend((!text.is_empty()).then_some(text)),
            HookOutput::Json(object) => {
                blocks.extend(block_reason(&object, &result.command.command));
                context.extend(additional_context(&object));
            }
        }
    }
    if blocks.is_empty() {
        PromptOutcome::Continue { context }
    } else {
        PromptOutcome::Block {
            reason: blocks.join("\n"),
        }
    }
}

/// Reads `SessionStart` results: the context they add. Nothing a `SessionStart` hook does blocks
/// the session.
pub fn session_context(results: &[HookResult]) -> Vec<String> {
    results
        .iter()
        .filter_map(|result| match read_output(result) {
            HookOutput::Text(text) => (!text.is_empty()).then_some(text),
            HookOutput::Json(object) => additional_context(&object),
            HookOutput::Block(stderr) => {
                log::warn!(
                    "SessionStart hook {:?} exited with code 2: {stderr}",
                    result.command.command
                );
                None
            }
            HookOutput::Failed { .. } => None,
        })
        .collect()
}

/// Reads `Stop` results. A failure never keeps the agent going, whatever its `onFailure`.
pub fn stop_outcome(results: &[HookResult]) -> StopOutcome {
    let reasons: Vec<String> = results
        .iter()
        .flat_map(|result| match read_output(result) {
            HookOutput::Block(reason) => vec![reason],
            HookOutput::Json(object) => block_reason(&object, &result.command.command)
                .into_iter()
                .chain(additional_context(&object))
                .collect(),
            HookOutput::Text(_) | HookOutput::Failed { .. } => Vec::new(),
        })
        .collect();
    if reasons.is_empty() {
        StopOutcome::Stop
    } else {
        StopOutcome::Continue {
            reason: reasons.join("\n"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookCommand;
    use std::time::Duration;

    fn exited(code: i32, stdout: &str, stderr: &str) -> HookResult {
        result(HookRun::Exited {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    fn result(run: HookRun) -> HookResult {
        HookResult {
            command: HookCommand {
                command: "hook.sh".into(),
                timeout: Duration::from_secs(5),
                blocks_on_failure: false,
            },
            run,
        }
    }

    fn blocking_on_failure(mut result: HookResult) -> HookResult {
        result.command.blocks_on_failure = true;
        result
    }

    fn permission(decision: &str, reason: &str) -> HookResult {
        exited(
            0,
            &format!(
                r#"{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"{decision}","permissionDecisionReason":"{reason}"}}}}"#
            ),
            "",
        )
    }

    #[test]
    fn deny_beats_ask_beats_allow() {
        let allow = permission("allow", "fine");
        let ask = permission("ask", "check");
        let deny = permission("deny", "no force-push");
        assert_eq!(
            tool_decision(std::slice::from_ref(&allow)),
            ToolDecision::Allow
        );
        assert_eq!(
            tool_decision(&[allow.clone(), ask.clone()]),
            ToolDecision::Ask
        );
        assert_eq!(
            tool_decision(&[allow, deny, ask]),
            ToolDecision::Deny {
                reason: "no force-push".into()
            }
        );
    }

    #[test]
    fn exit_2_denies_with_stderr() {
        assert_eq!(
            tool_decision(&[exited(2, "", "outside the worktree\n")]),
            ToolDecision::Deny {
                reason: "outside the worktree".into()
            }
        );
    }

    #[test]
    fn deprecated_approve_and_block_map_to_allow_and_deny() {
        assert_eq!(
            tool_decision(&[exited(0, r#"{"decision":"approve"}"#, "")]),
            ToolDecision::Allow
        );
        assert_eq!(
            tool_decision(&[exited(0, r#"{"decision":"block","reason":"no"}"#, "")]),
            ToolDecision::Deny {
                reason: "no".into()
            }
        );
    }

    #[test]
    fn exit_0_plain_text_is_no_decision() {
        assert_eq!(
            tool_decision(&[exited(0, "checked\n", "")]),
            ToolDecision::Undecided
        );
    }

    #[test]
    fn invalid_json_is_no_decision() {
        assert_eq!(
            tool_decision(&[exited(0, r#"{"hookSpecificOutput": }"#, "")]),
            ToolDecision::Undecided
        );
    }

    #[test]
    fn timeout_and_other_exit_codes_are_no_decision() {
        assert_eq!(
            tool_decision(&[result(HookRun::TimedOut), exited(1, "", "crashed")]),
            ToolDecision::Undecided
        );
    }

    #[test]
    fn on_failure_block_denies_on_timeout_failure_or_invalid_json() {
        for run in [
            result(HookRun::TimedOut),
            result(HookRun::Failed("No such file or directory".into())),
            exited(1, "", "crashed"),
            exited(0, "{not json}", ""),
        ] {
            assert!(
                matches!(
                    tool_decision(&[blocking_on_failure(run.clone())]),
                    ToolDecision::Deny { .. }
                ),
                "{run:?}"
            );
        }
    }

    #[test]
    fn plain_stdout_is_context_for_prompt_events() {
        let results = [exited(0, "branch: main\n", ""), exited(0, "", "")];
        assert_eq!(
            prompt_outcome(&results),
            PromptOutcome::Continue {
                context: vec!["branch: main".into()]
            }
        );
        assert_eq!(session_context(&results), ["branch: main"]);
    }

    #[test]
    fn additional_context_from_json() {
        let results = [exited(
            0,
            r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"ticket ABC-1"}}"#,
            "",
        )];
        assert_eq!(
            prompt_outcome(&results),
            PromptOutcome::Continue {
                context: vec!["ticket ABC-1".into()]
            }
        );
        assert_eq!(session_context(&results), ["ticket ABC-1"]);
    }

    #[test]
    fn prompt_block_carries_reason() {
        assert_eq!(
            prompt_outcome(&[exited(
                0,
                r#"{"decision":"block","reason":"secret in prompt"}"#,
                ""
            )]),
            PromptOutcome::Block {
                reason: "secret in prompt".into()
            }
        );
        assert_eq!(
            prompt_outcome(&[exited(2, "", "secret in prompt")]),
            PromptOutcome::Block {
                reason: "secret in prompt".into()
            }
        );
    }

    #[test]
    fn nothing_blocks_session_start() {
        let results = [
            exited(2, "", "refused"),
            blocking_on_failure(result(HookRun::TimedOut)),
        ];
        assert!(session_context(&results).is_empty());
    }

    #[test]
    fn stop_block_carries_reason() {
        assert_eq!(
            stop_outcome(&[exited(
                0,
                r#"{"decision":"block","reason":"run the tests"}"#,
                ""
            )]),
            StopOutcome::Continue {
                reason: "run the tests".into()
            }
        );
        assert_eq!(
            stop_outcome(&[exited(2, "", "run the tests")]),
            StopOutcome::Continue {
                reason: "run the tests".into()
            }
        );
    }

    #[test]
    fn stop_additional_context_continues() {
        assert_eq!(
            stop_outcome(&[exited(
                0,
                r#"{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"update the spec"}}"#,
                ""
            )]),
            StopOutcome::Continue {
                reason: "update the spec".into()
            }
        );
    }

    #[test]
    fn stop_failures_let_the_agent_stop() {
        assert_eq!(
            stop_outcome(&[
                exited(0, "done\n", ""),
                blocking_on_failure(result(HookRun::TimedOut))
            ]),
            StopOutcome::Stop
        );
    }
}
