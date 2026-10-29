# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

### How this fork differs

This fork adds client-side hooks for external ACP agents, in
[Claude Code's hook format](https://code.claude.com/docs/en/hooks), so a hook script written for
Claude Code runs unchanged under any ACP agent Zed starts. Apart from that and its build
workflows, it is upstream Zed.

Hooks are set per agent in your user settings, never a project's, since they run arbitrary
commands:

```json
{
  "agent_servers": {
    "my-agent": {
      "type": "custom",
      "command": "my-agent",
      "args": ["acp"],
      "hooks": {
        "PreToolUse": [
          { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/hooks/guard.sh" }] }
        ]
      }
    }
  }
}
```

| Event | When it runs |
|---|---|
| `PreToolUse` (`Bash`) | Before Zed starts a terminal command the agent asked for; a deny refuses it |
| `PreToolUse` (`Write`) | Before Zed writes a file the agent sent through `fs/write_text_file`; a deny refuses it |
| `UserPromptSubmit` | Before a prompt is sent; it can block the prompt or add context ahead of it |
| `SessionStart` | When a session starts or is resumed; its output goes ahead of the first prompt |
| `Stop` | When the agent ends a turn; a block sends its reason back as the next prompt |

Not supported: other events, hook types other than `command`, and hooks that set `args`, `if`,
`shell` or `async`, which are skipped. An `ask` decision refuses the tool, since a terminal or
file write carries no prompt to answer it on. File reads aren't hooked, and neither are tool
calls the agent runs itself, such as MCP calls, web fetches or files it writes without Zed.
Hooks run through `sh`, on macOS and Linux.

Branches: `build` (the default) holds this README and the fork's workflows, `acp-hooks` holds
the hooks on top of upstream Zed. Builds come from the fork's manually run workflows:
`fork_test_hooks.yml` tests a branch and `fork_bundle_mac.yml` builds a macOS app from one.

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
