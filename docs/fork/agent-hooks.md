# Hooks for external ACP agents: design

How this fork's hooks work and why, for anyone changing them or proposing them upstream. The
README's "How this fork differs" is the short version. The code is on the `acp-hooks` branch,
mostly in `crates/agent_servers/src/hooks.rs`, `hooks/` and `acp/v1.rs`.

## Decisions

1. **Claude Code's hook format.** Hooks are configured per agent as `agent_servers.<name>.hooks`,
   in the same shape as Claude Code's `hooks` setting: event, matcher groups, `command` hooks
   with a `timeout`. Each hook gets Claude Code's stdin JSON and is read by its rules. Exit 2
   blocks where the event can block, with stderr as the reason; exit 0 with JSON uses
   `hookSpecificOutput`. Hooks run with `/bin/sh -c` in the session's working directory, with
   `CLAUDE_PROJECT_DIR` set to it. Reason: one hook script serves both tools, and the format is
   documented ([code.claude.com/docs/en/hooks](https://code.claude.com/docs/en/hooks)) rather
   than invented.
2. **User settings only.** Hooks run arbitrary commands, so a project's `.zed/settings.json`
   can't add them: a cloned repository must not run code when it is opened. Release-channel
   overrides such as `"dev"` also exist only in user settings.
3. **Five hook points.**

   | Event | Where Zed runs it | Effect |
   |---|---|---|
   | `PreToolUse`, shell | Before Zed starts a terminal the agent asked for | A deny returns an ACP error and nothing runs. `tool_name: "Bash"`, `tool_input.command` as POSIX shell text |
   | `PreToolUse`, file write | Before Zed writes a file the agent sent with `fs/write_text_file` | A deny returns an ACP error and nothing is written. `tool_name: "Write"`, `tool_input` `{file_path, content}` |
   | `UserPromptSubmit` | Before a prompt is sent | Exit 2 drops the prompt; `additionalContext` goes in a text block ahead of the user's text |
   | `SessionStart` | When a session is created (`startup`) or loaded (`resume`) | Its output goes ahead of the session's first prompt, since ACP has no system-prompt slot |
   | `Stop` | After the agent ends its turn | A block sends its `reason` as the next prompt, with `stop_hook_active: true`; at most 8 follow-ups, as in Claude Code |

   A cancel that arrives while `UserPromptSubmit` or `Stop` hooks run ends the turn before
   anything more reaches the agent.
4. **Agent-neutral and upstreamable.** Nothing is specific to one agent. If Zed ships hooks in
   another format, the fork follows Zed's.
5. **No hooks on permission requests.** Every shell command and every write an agent sends
   through Zed passes through terminal creation and `fs/write_text_file`. Hooks at a permission
   request would miss what an agent's policy doesn't ask about and would run shell hooks twice.
6. **A hooks module apart from the ACP client.** The module turns an event into a decision; the
   v1 client calls it at each hook point, so moving to the v2 client means moving those calls.
7. **`ask` refuses.** Neither a terminal request nor a file write carries a tool call to show a
   prompt on, so an `ask` fails closed. The same refusals apply when there is no working
   directory, in a remote project, or for a path that isn't absolute or contains `..`.
8. **Unsupported hooks are skipped, never widened.** Event names and hook types stay strings, so
   a whole Claude Code `hooks` block loads. A hook with `args`, `if`, `shell` or `async`, or an
   `onFailure` other than `continue` or `block`, is skipped with a log line. These fields are
   read as raw JSON, so a value of the wrong type still counts as set. `onFailure: "block"` is
   honoured.
9. **Hooks follow settings live.** A hook added or removed applies to the next event without
   reconnecting the agent. While the settings have no entry for the agent (renamed, removed, or
   lost to a settings error), its connection keeps the hooks it last had.
10. **A hooked terminal runs exactly the text its hooks approved.** The command, its quoted
    arguments and an `export` of its environment become one POSIX shell string; hooks read it,
    and Zed runs that string in the directory the hooks were given. The terminal applies only the
    user's terminal settings, not the project's, activates no Python environment, and starts its
    shell non-interactively. It fails closed for a shell Zed knows isn't POSIX, an environment
    name that isn't an identifier, a NUL byte, a relative directory or a remote project. Reason:
    reviews found the quoting, the environment, shell startup files, project settings and a
    virtualenv's activate script could each make what ran differ from what a hook read.
11. **A Write hook judges the file the write lands on and the text that is saved.** It sees the
    path as the filesystem resolves it (on-disk case, symlinks followed), and a path through a
    symlink to a missing file is refused. It sees the text with line breaks normalised, as the
    buffer stores it. A hooked write is refused when the buffer changed since the agent last
    read it through Zed, so two approved writes can't merge into text no hook saw.
12. **An agent with hooks stays on ACP v1.** The v2 client runs no hooks yet, so an agent with any
    hook connects over v1 rather than silently losing them.
13. **A refusal carries its reason in the error message.** Agents show an ACP error's message,
    not its data, so a refused terminal or write returns code -32603 with the message
    `Zed's PreToolUse hooks block this: <reason>`, the reason cut to 4096 characters, and the
    whole reason as the data.

## What it can't do

- **Hooks see only what passes through Zed:** shell commands it runs and files it writes. MCP
  calls, web fetches, an agent's own reads and files an agent writes without Zed never reach a
  hook. Every edit arrives as `Write` with the whole new text, never as `Edit`.
- **Paths:** the part of a path that doesn't exist yet isn't case-corrected, so a guard on a
  case-insensitive disk must compare names without regard to case. A symlink swapped between
  the hook and the write, or a hard link, isn't seen.
- **A hook that crashes or times out lets the action through** unless it sets
  `onFailure: "block"`. A hook printing more than 1 MiB on stdout counts as failed.
- **The saved file can differ from `content`** by formatting on save, the buffer's line endings
  and its encoding.
- **A hooked terminal still gets its directory's login-shell environment**, as direnv or mise load
  it, and that environment can carry code (exported bash functions, `SHELLOPTS`).
- **No `PostToolUse`, `PreCompact` or subagent events**; no `transcript_path`, `tool_use_id` or
  `last_assistant_message`; the universal output fields (`continue`, `systemMessage`,
  `updatedInput` and the rest) are ignored.
- **`UserPromptSubmit`'s `prompt` is the typed text only**, without @-mentions or images.
- **Injected context shows on resume**, because the agent replays history with it.
- **The Stop cap counts per user prompt**, and a cancel doesn't stop a hook already running.
- **Matchers are Rust regular expressions**: a lookaround doesn't compile, and its group is skipped.
- **Hooks run on Zed's machine**, so a remote project's terminals and writes are refused.
- **macOS and Linux only:** hooks run through `/bin/sh`, so on Windows they don't start.
- **Nothing limits how many hooks run at once.**
