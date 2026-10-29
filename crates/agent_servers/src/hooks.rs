mod decision;
mod runner;

pub use decision::{PromptOutcome, ToolDecision};

use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};

use collections::HashMap;
use gpui::{App, BackgroundExecutor, Subscription};
use project::{AgentId, agent_server_store::AllAgentServersSettings};
use regex::Regex;
use serde_json::{Map, Value, json};
use settings::{AgentHookContent, AgentHooksContent, SettingsStore};

/// The Claude Code hook events Zed runs for external agents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookEvent {
    PreToolUse,
    UserPromptSubmit,
    SessionStart,
    Stop,
}

impl HookEvent {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "PreToolUse" => Some(Self::PreToolUse),
            "UserPromptSubmit" => Some(Self::UserPromptSubmit),
            "SessionStart" => Some(Self::SessionStart),
            "Stop" => Some(Self::Stop),
            _ => None,
        }
    }

    /// The event's name in settings and in `hook_event_name`.
    pub fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionStart => "SessionStart",
            Self::Stop => "Stop",
        }
    }

    fn has_matcher(self) -> bool {
        matches!(self, Self::PreToolUse | Self::SessionStart)
    }

    fn default_timeout(self) -> Duration {
        match self {
            Self::UserPromptSubmit => Duration::from_secs(30),
            _ => Duration::from_secs(600),
        }
    }
}

/// One `command` hook, ready to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookCommand {
    /// The shell command, run with `sh -c`.
    pub command: String,
    pub timeout: Duration,
    /// Whether a failure or timeout blocks the action, as exit 2 does (`onFailure: "block"`).
    pub blocks_on_failure: bool,
}

impl HookCommand {
    fn from_settings(hook: &AgentHookContent, event: HookEvent) -> Option<Self> {
        let unsupported = if hook.hook_type != "command" {
            Some(format!("type {:?}", hook.hook_type))
        } else if hook.args.is_some() {
            Some("`args`".to_string())
        } else if hook.condition.is_some() {
            Some("`if`".to_string())
        } else if hook.shell.is_some() {
            Some("`shell`".to_string())
        } else if hook.run_async == Some(true) {
            Some("`async`".to_string())
        } else {
            None
        };
        if let Some(unsupported) = unsupported {
            log::warn!(
                "skipping a {} hook: {unsupported} is not supported",
                event.name()
            );
            return None;
        }
        let Some(command) = hook.command.clone() else {
            log::warn!("skipping a {} hook without a `command`", event.name());
            return None;
        };
        let blocks_on_failure = match hook.on_failure.as_deref() {
            None | Some("continue") => false,
            Some("block") => true,
            Some(other) => {
                log::warn!(
                    "skipping {} hook {command:?}: `onFailure` must be \"continue\" or \"block\", not {other:?}",
                    event.name()
                );
                return None;
            }
        };
        let timeout = hook
            .timeout
            .map_or_else(|| event.default_timeout(), Duration::from_secs);
        Some(Self {
            command,
            timeout,
            blocks_on_failure,
        })
    }
}

#[derive(Clone, Debug)]
enum Matcher {
    All,
    Exact(Vec<String>),
    Pattern(Regex),
}

impl Matcher {
    fn parse(matcher: &str) -> Result<Self, regex::Error> {
        if matcher.is_empty() || matcher == "*" {
            return Ok(Self::All);
        }
        let is_name_list = matcher
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ' | ',' | '|'));
        if is_name_list {
            let names = matcher
                .split(['|', ','])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect();
            Ok(Self::Exact(names))
        } else {
            Regex::new(matcher).map(Self::Pattern)
        }
    }

    fn matches(&self, subject: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact(names) => names.iter().any(|name| name == subject),
            Self::Pattern(pattern) => pattern.is_match(subject),
        }
    }
}

#[derive(Clone, Debug)]
struct MatcherGroup {
    matcher: Matcher,
    commands: Vec<HookCommand>,
}

/// An agent's hooks by event, with the ones Zed can't run left out.
#[derive(Clone, Debug, Default)]
pub struct AgentHooks {
    groups: HashMap<HookEvent, Vec<MatcherGroup>>,
}

impl AgentHooks {
    /// Reads an agent's `hooks` setting, logging and skipping unsupported events, hook types and
    /// fields, and matcher groups whose regular expression doesn't compile.
    pub fn from_settings(content: &AgentHooksContent) -> Self {
        let mut groups: HashMap<HookEvent, Vec<MatcherGroup>> = HashMap::default();
        for (event_name, matcher_groups) in content {
            let Some(event) = HookEvent::from_name(event_name) else {
                log::warn!("skipping agent hooks for {event_name:?}: Zed doesn't run that event");
                continue;
            };
            for group in matcher_groups {
                let matcher = if event.has_matcher() {
                    let pattern = group.matcher.as_deref().unwrap_or_default();
                    match Matcher::parse(pattern) {
                        Ok(matcher) => matcher,
                        Err(error) => {
                            log::warn!("skipping {event_name} hooks matching {pattern:?}: {error}");
                            continue;
                        }
                    }
                } else {
                    Matcher::All
                };
                let commands: Vec<_> = group
                    .hooks
                    .iter()
                    .filter_map(|hook| HookCommand::from_settings(hook, event))
                    .collect();
                if !commands.is_empty() {
                    groups
                        .entry(event)
                        .or_default()
                        .push(MatcherGroup { matcher, commands });
                }
            }
        }
        Self { groups }
    }

    /// The hooks to run for `event` on `subject`: the tool name for `PreToolUse`, the session
    /// source for `SessionStart`, ignored for events without matchers.
    pub fn matching(&self, event: HookEvent, subject: &str) -> Vec<HookCommand> {
        self.groups
            .get(&event)
            .into_iter()
            .flatten()
            .filter(|group| group.matcher.matches(subject))
            .flat_map(|group| group.commands.iter().cloned())
            .collect()
    }
}

/// An agent's hooks as the user's settings have them now.
#[derive(Clone, Default)]
pub struct LiveAgentHooks(Rc<RefCell<LoadedHooks>>);

#[derive(Default)]
struct LoadedHooks {
    content: AgentHooksContent,
    hooks: AgentHooks,
}

impl LiveAgentHooks {
    /// Loads `agent_id`'s hooks from the user's settings, and again each time settings change,
    /// until the returned subscription is dropped.
    pub fn observe_settings(&self, agent_id: AgentId, cx: &mut App) -> Subscription {
        if cx.try_global::<SettingsStore>().is_none() {
            return Subscription::new(|| {});
        }

        self.refresh_from_settings(&agent_id, cx);
        let hooks = self.clone();
        cx.observe_global::<SettingsStore>(move |cx| {
            hooks.refresh_from_settings(&agent_id, cx);
        })
    }

    fn refresh_from_settings(&self, agent_id: &AgentId, cx: &App) {
        let Some(settings_store) = cx.try_global::<SettingsStore>() else {
            return;
        };
        let content = settings_store
            .get::<AllAgentServersSettings>(None)
            .get(agent_id.as_ref())
            .map(|settings| settings.hooks().clone())
            .unwrap_or_default();
        let mut loaded = self.0.borrow_mut();
        // Unchanged hooks aren't rebuilt, so their warnings aren't logged on every settings change.
        if loaded.content != content {
            loaded.hooks = AgentHooks::from_settings(&content);
            loaded.content = content;
        }
    }

    /// See [`AgentHooks::matching`].
    pub fn matching(&self, event: HookEvent, subject: &str) -> Vec<HookCommand> {
        self.0.borrow().hooks.matching(event, subject)
    }
}

/// Runs `PreToolUse` hooks in `project_dir` for a shell command the agent asked Zed to start in
/// `cwd`, as Claude Code runs them for its `Bash` tool.
pub async fn shell_command_decision(
    commands: Vec<HookCommand>,
    session_id: &str,
    project_dir: &Path,
    cwd: &Path,
    command_line: &str,
    executor: &BackgroundExecutor,
) -> ToolDecision {
    let mut fields = Map::new();
    fields.insert("tool_name".into(), "Bash".into());
    fields.insert("tool_input".into(), json!({ "command": command_line }));
    let input = hook_input(HookEvent::PreToolUse, session_id, cwd, fields);
    decision::tool_decision(&runner::run_hooks(commands, &input, project_dir, executor).await)
}

/// How a session came to be opened, as `SessionStart` hooks are told it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSource {
    Startup,
    Resume,
}

impl SessionSource {
    /// The `source` Claude Code sends, which `SessionStart` matchers match.
    pub fn name(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
        }
    }
}

/// Runs `SessionStart` hooks in `project_dir` and returns the context they add.
pub async fn session_start_context(
    commands: Vec<HookCommand>,
    session_id: &str,
    project_dir: &Path,
    source: SessionSource,
    executor: &BackgroundExecutor,
) -> Vec<String> {
    let mut fields = Map::new();
    fields.insert("source".into(), source.name().into());
    let input = hook_input(HookEvent::SessionStart, session_id, project_dir, fields);
    decision::session_context(&runner::run_hooks(commands, &input, project_dir, executor).await)
}

/// Runs `UserPromptSubmit` hooks in `project_dir` on the text of the prompt the user sent.
pub async fn submitted_prompt_outcome(
    commands: Vec<HookCommand>,
    session_id: &str,
    project_dir: &Path,
    prompt: &str,
    executor: &BackgroundExecutor,
) -> PromptOutcome {
    let mut fields = Map::new();
    fields.insert("prompt".into(), prompt.into());
    let input = hook_input(HookEvent::UserPromptSubmit, session_id, project_dir, fields);
    decision::prompt_outcome(&runner::run_hooks(commands, &input, project_dir, executor).await)
}

/// The JSON a hook reads on stdin: `fields` plus Claude Code's common `session_id`, `cwd` and
/// `hook_event_name`. There is no `transcript_path`, since a Zed thread isn't a Claude Code
/// transcript.
fn hook_input(
    event: HookEvent,
    session_id: &str,
    cwd: &Path,
    mut fields: Map<String, Value>,
) -> Value {
    fields.insert("session_id".into(), session_id.into());
    fields.insert("cwd".into(), cwd.to_string_lossy().into());
    fields.insert("hook_event_name".into(), event.name().into());
    Value::Object(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks(settings: Value) -> AgentHooks {
        AgentHooks::from_settings(&serde_json::from_value(settings).expect("hooks settings"))
    }

    fn commands(hooks: &AgentHooks, event: HookEvent, subject: &str) -> Vec<String> {
        hooks
            .matching(event, subject)
            .into_iter()
            .map(|hook| hook.command)
            .collect()
    }

    fn pre_tool_use(matcher: Value) -> AgentHooks {
        hooks(json!({
            "PreToolUse": [{ "matcher": matcher, "hooks": [{ "type": "command", "command": "guard" }] }]
        }))
    }

    #[test]
    fn star_empty_and_missing_match_all() {
        for matcher in [json!("*"), json!(""), Value::Null] {
            let hooks = pre_tool_use(matcher.clone());
            assert_eq!(
                commands(&hooks, HookEvent::PreToolUse, "Bash"),
                ["guard"],
                "{matcher}"
            );
        }
    }

    #[test]
    fn pipe_and_comma_lists_match_exactly() {
        for matcher in ["Edit|Write", "Edit, Write"] {
            let hooks = pre_tool_use(json!(matcher));
            assert_eq!(commands(&hooks, HookEvent::PreToolUse, "Write"), ["guard"]);
            assert!(commands(&hooks, HookEvent::PreToolUse, "NotebookEdit").is_empty());
        }
        let hooks = pre_tool_use(json!("code-reviewer"));
        assert_eq!(
            commands(&hooks, HookEvent::PreToolUse, "code-reviewer"),
            ["guard"]
        );
        assert!(commands(&hooks, HookEvent::PreToolUse, "code").is_empty());
    }

    #[test]
    fn other_characters_are_unanchored_regex() {
        let hooks = pre_tool_use(json!("mcp__memory__.*"));
        assert_eq!(
            commands(&hooks, HookEvent::PreToolUse, "mcp__memory__read"),
            ["guard"]
        );
        let hooks = pre_tool_use(json!("Edit.*"));
        assert_eq!(
            commands(&hooks, HookEvent::PreToolUse, "NotebookEdit"),
            ["guard"]
        );
    }

    #[test]
    fn a_matcher_that_does_not_compile_skips_only_its_group() {
        let hooks = hooks(json!({
            "PreToolUse": [
                { "matcher": "(?<=a)b", "hooks": [{ "type": "command", "command": "lookbehind" }] },
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard" }] }
            ]
        }));
        assert_eq!(commands(&hooks, HookEvent::PreToolUse, "Bash"), ["guard"]);
    }

    #[test]
    fn events_without_matchers_ignore_the_matcher() {
        let hooks = hooks(json!({
            "Stop": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "check" }] }]
        }));
        assert_eq!(commands(&hooks, HookEvent::Stop, ""), ["check"]);
    }

    #[test]
    fn unsupported_events_and_types_are_skipped() {
        let hooks = hooks(json!({
            "PostToolUse": [{ "hooks": [{ "type": "command", "command": "after" }] }],
            "PreToolUse": [{ "hooks": [
                { "type": "http", "url": "http://localhost" },
                { "type": "command" },
                { "type": "command", "command": "guard" }
            ] }]
        }));
        assert_eq!(commands(&hooks, HookEvent::PreToolUse, "Bash"), ["guard"]);
    }

    #[test]
    fn hooks_with_args_if_shell_or_async_are_skipped() {
        let hooks = hooks(json!({
            "PreToolUse": [{ "hooks": [
                { "type": "command", "command": "exec-form", "args": ["x"] },
                { "type": "command", "command": "conditional", "if": "Bash(git *)" },
                { "type": "command", "command": "powershell", "shell": "powershell" },
                { "type": "command", "command": "background", "async": true },
                { "type": "command", "command": "unknown-failure", "onFailure": "retry" },
                { "type": "command", "command": "guard", "async": false }
            ] }]
        }));
        assert_eq!(commands(&hooks, HookEvent::PreToolUse, "Bash"), ["guard"]);
    }

    #[test]
    fn default_timeout_is_600_or_30_on_user_prompt_submit() {
        let hook = json!([{ "hooks": [{ "type": "command", "command": "x" }] }]);
        let hooks = hooks(json!({ "Stop": hook, "UserPromptSubmit": hook }));
        assert_eq!(
            hooks.matching(HookEvent::Stop, "")[0].timeout,
            Duration::from_secs(600)
        );
        assert_eq!(
            hooks.matching(HookEvent::UserPromptSubmit, "")[0].timeout,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn timeout_and_on_failure_are_read() {
        let hooks = hooks(json!({
            "Stop": [{ "hooks": [{ "type": "command", "command": "x", "timeout": 5, "onFailure": "block" }] }]
        }));
        assert_eq!(
            hooks.matching(HookEvent::Stop, ""),
            [HookCommand {
                command: "x".into(),
                timeout: Duration::from_secs(5),
                blocks_on_failure: true,
            }]
        );
    }

    #[test]
    fn hook_input_has_common_fields_and_no_transcript_path() {
        let mut fields = Map::new();
        fields.insert("prompt".into(), "hello".into());
        let input = hook_input(
            HookEvent::UserPromptSubmit,
            "session-1",
            Path::new("/work"),
            fields,
        );
        assert_eq!(
            input,
            json!({
                "session_id": "session-1",
                "cwd": "/work",
                "hook_event_name": "UserPromptSubmit",
                "prompt": "hello"
            })
        );
    }
}
