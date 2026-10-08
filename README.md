# pane-relay

[![license: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![herdr plugin](https://img.shields.io/badge/herdr-plugin-8ec07c)](https://herdr.dev/plugins/)

[日本語](README.ja.md)

A [Herdr](https://herdr.dev) plugin that takes the last finished answer of the AI in one pane (A), adds your instruction, and submits both to the AI in another pane (B).
Supported agents: Claude Code and Codex, on either side.

## Usage

1. Focus A and press the key you bound to the plugin.
2. A popup opens and reads A's last answer from its session transcript.
3. Pick B from every other pane, in every workspace (A's tab first, then its workspace, then the others; the workspace is shown when the list spans several). Panes that cannot receive (busy, waiting for approval, no registered session…) are listed with the reason and cannot be picked.
   ↑↓ / `Ctrl+p` `Ctrl+n` / `j` `k` move, `Space` or `1`–`9` pick, `Enter` confirms.
4. Type the instruction. `Enter` sends, `Alt+Enter` inserts a line break, `Ctrl+]` picks B again, `Ctrl+r` re-reads the answer, `Esc` / `Ctrl+g` / `Ctrl+q` quit (in the target list too).
   Committing IME input or pasting several lines never sends.
5. Right before sending, both panes are checked again (same agent session, idle or done) and the answer is read again. If A's answer changed, nothing is sent: the new answer is loaded, your instruction is kept, and you decide again.
6. B receives one prompt, submitted once: your instruction, a short label, and A's answer verbatim between fence lines that do not occur in it.

"Sent" means Herdr accepted the input, not that B finished working on it.

## What "the last finished answer" means

The answer comes from the agent's own transcript (`~/.claude/projects`, `~/.codex/sessions`): the final answer of the latest turn, and only if that turn ended normally. Progress messages, thinking, tool calls and tool results are left out. There is no manual selection and no extra AI call.

Nothing is sent, and the reason is shown, when the latest turn is still running, was interrupted, failed or is empty; when the transcript is still being written (waits up to 3 s); when Herdr has no session for the pane, or the transcript is missing or ambiguous; or when the transcript has a shape that was not verified. An older answer is never sent instead.

## Requirements and limits

- A and B must be idle or done in Herdr.
- The Herdr integration must be installed (`herdr integration install claude` / `codex`, then restart the agent). Codex registers its session only after its first prompt, so send it something once before using it (as A or B).
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
- Verified with Claude Code 2.1.293 and Codex CLI 0.160.1
- macOS (verified); Linux not verified
- Rust 1.85 or later to build

## Install

```sh
git clone <this repository> pane-relay
cd pane-relay
sh scripts/build.sh
herdr plugin link .
```

Bind a key in `~/.config/herdr/config.toml` (the key is an example), then run `herdr server reload-config`:

```toml
[[keys.command]]
key = "prefix+h"
type = "plugin_action"
command = "abroller666.pane-relay.open"
description = "hand the last answer to another pane"
```

## Configuration

For non-standard Claude Code / Codex directories, put a `config.json` in the plugin's config directory (`HERDR_PLUGIN_CONFIG_DIR`):

```json
{"claude_roots": ["~/.claude/projects"], "codex_roots": ["~/.codex/sessions"], "max_payload_bytes": 262144}
```

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
