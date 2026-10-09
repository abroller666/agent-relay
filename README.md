# agent-relay

[![license: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![herdr plugin](https://img.shields.io/badge/herdr-plugin-8ec07c)](https://herdr.dev/plugins/)

[日本語](README.ja.md)

A [Herdr](https://herdr.dev) plugin that takes the last finished answer of the AI in one pane (A), adds your instruction, and submits both to the AI in another pane (B).
Supported agents: Claude Code and Codex, on either side.

## Supported agents and languages

| Agent | As A (source) | As B (target) | Verified with |
|---|---|---|---|
| Claude Code | yes | yes | 2.1.293 (2.1.219 and older: no, they do not record turn completion) |
| Codex CLI | yes | yes | 0.160.1, 0.161.0; both the default shared daemon and `--no-daemon` |

Other agents Herdr detects are not supported: they are listed as targets with the reason and cannot be picked, and an answer is never read from them.

| What | Language |
|---|---|
| Popup (menus, messages) | English |
| Line introducing the quote in the prompt sent to B | English (default) or Japanese, set by `prompt_language` (see [Configuration](#configuration)) |
| Your instruction and A's answer | Any; sent as is (Unicode, including IME input) |
| README | English, [Japanese](README.ja.md) |

## Usage

1. Focus A and press the key you bound to the plugin.
2. A popup opens and reads A's last answer from its session transcript.
3. Pick B from every other pane running an AI agent, in every workspace (A's tab first, then its workspace, then the others; the workspace is shown when the list spans several). Panes that cannot receive (busy, waiting for approval, no registered session…) are listed with the reason and cannot be picked.
   ↑↓ / `Ctrl+p` `Ctrl+n` / `j` `k` move, `Space` or `1`–`9` pick, `Enter` confirms.
4. Type the instruction (it may be left empty: then only the label and the answer are sent). `Enter` sends, `Alt+Enter` inserts a line break, `Ctrl+]` picks B again, `Ctrl+o` chooses which answer to send, `Ctrl+r` re-reads the latest answer, `Esc` / `Ctrl+g` / `Ctrl+q` quit (in the target list too).
   Committing IME input or pasting several lines never sends.
   `Ctrl+o` lists A's finished answers of the current conversation (newest first, up to 50, with time, first line and size); pick one with a number or ↑↓ and `Enter`, or leave with `Ctrl+o`. Without a pick the latest answer is sent. Rewound and interrupted turns are not listed; an interrupted latest turn does not hide older answers. Answers from before a `/compact` are listed too.
5. Right before sending, both panes are checked again (same agent session, idle or done) and the answer is read again. For the latest answer, if A's answer changed, nothing is sent: the new answer is loaded, your instruction is kept, and you decide again. An answer picked with `Ctrl+o` is sent even if newer answers appeared, as long as it is still part of the conversation.
6. B receives one prompt, submitted once: your instruction, a short label, and A's answer verbatim between fence lines that do not occur in it.

The popup closes as soon as Herdr accepts the input; that says nothing about B finishing. Only when delivery cannot be confirmed does the popup stay open with a warning.

## What "the last finished answer" means

The answer comes from the agent's own transcript (`~/.claude/projects`, `~/.codex/sessions`): the final answer of the latest turn, and only if that turn ended normally. Progress messages, thinking, tool calls and tool results are left out. There is no manual selection and no extra AI call.

Nothing is sent, and the reason is shown, when the latest turn is still running, was interrupted, failed or is empty; when the transcript is still being written (waits up to 3 s); when Herdr has no session for the pane, or the transcript is missing or ambiguous; or when the transcript has a shape that was not verified. An older answer is never sent instead.

## Requirements and limits

- A and B must be idle or done in Herdr.
- The Herdr integration must be installed (`herdr integration install claude` / `codex`, then restart the agent). Codex registers its session only after its first prompt, so send it something once before using it (as A or B).
- **Codex on its shared background daemon (the default) is supported.** There Herdr cannot register Codex sessions reliably (openai/codex#48500, herdrdev/herdr#4649), so a Codex pane is bound to the one daemon thread whose name and working directory match the pane's own terminal title ("<thread name> | <project>"). Nothing is sent before the first prompt (no thread name yet), when two threads in the directory share the name, or with a custom `tui.terminal_title`; `codex --no-daemon` then works through Herdr as before. The daemon interface Codex offers is experimental and may change.
- Keep B's input box empty: text left there is joined in front of the prompt. The plugin does not clear it.
- **Right after a Claude Code rewind (`Esc` `Esc`), until the next prompt, the answer from before the rewind is sent.** A rewind leaves no trace in the transcript. Check the preview.
- Right after a Codex rewind or fork, there is no answer until the next prompt.
- After a Claude Code slash command such as `/model`, completion cannot be confirmed until the next prompt.
- Checks and send are not atomic; a pane can still change right after the last check (Herdr has no conditional send).
- If the result of a send cannot be confirmed (connection lost, timeout), the popup says so and **does not resend**. Look at B.
- Up to 256 KiB (instruction plus answer) by default; larger prompts are refused, never cut. Codex was verified with 64 KiB.
- Answers containing terminal control characters are refused (they could drive B's terminal).
- Same machine and Herdr server only; no SSH, containers or other users.

Details: [docs/compatibility.md](docs/compatibility.md).

## Requirements

- Herdr 0.9.3 or later
- Claude Code 2.1.293 / Codex CLI 0.160.1 or 0.161.0 verified (see [Supported agents and languages](#supported-agents-and-languages))
- macOS (verified); Linux not verified
- Rust 1.85 or later to build

## Install

```sh
git clone <this repository> agent-relay
cd agent-relay
sh scripts/build.sh
herdr plugin link .
```

Bind a key in `~/.config/herdr/config.toml` (the key is an example), then run `herdr server reload-config`:

```toml
[[keys.command]]
key = "prefix+h"
type = "plugin_action"
command = "abroller666.agent-relay.open"
description = "hand the last answer to another pane"
```

## Configuration

Put a `config.json` in the plugin's config directory (`HERDR_PLUGIN_CONFIG_DIR`). Every key is optional, and an unknown key is an error:

```json
{
  "prompt_language": "en",
  "claude_roots": ["~/.claude/projects"],
  "codex_roots": ["~/.codex/sessions"],
  "max_payload_bytes": 262144,
  "max_file_bytes": 268435456,
  "max_line_bytes": 8388608,
  "max_candidates": 10000
}
```

- `prompt_language`: the language of the line introducing the quote, `"en"` (the default) or `"ja"`. It only changes the prompt sent to B; the popup stays in English.
- `claude_roots`, `codex_roots`: where agent-relay looks for the transcripts. Claude Code and Codex decide where they write them; agent-relay only reads them and does not change that. The defaults are the standard locations. Set these only if you moved them (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`); the popup does not read those variables, since its environment may differ from the agents'.
- `max_payload_bytes`: the largest prompt to send. `max_file_bytes`, `max_line_bytes`, `max_candidates`: read limits for transcripts.

## Data

Transcripts are only read. Each launch keeps its state (including the answer and instruction) in a private file (0600, directory 0700) that is removed when the popup closes; leftovers are removed after 24 hours. Answers and instructions are never logged.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
sh scripts/build.sh
```

Key handling and the pane list follow [broadcast-pane](https://github.com/abroller666/broadcast-pane) (MIT).

## License

MIT
