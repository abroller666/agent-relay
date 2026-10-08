#!/usr/bin/env python3
"""Reduce a Claude Code or Codex transcript to a fixture.

Keeps the record structure the adapters read (types, ids, parent links,
phases, completion events, ordinals, history_base) and drops everything
else: account ids, instructions, tool inputs and outputs, reasoning,
encrypted content. Text blocks are kept, so only run this on sessions that
were driven with synthetic prompts. Paths given with --replace are
rewritten in what is kept.

    sanitize.py claude|codex SRC DEST [--replace OLD=NEW ...]
"""

import argparse
import json

REDACTED = "<redacted>"


def claude_record(r):
    t = r.get("type")
    if t not in ("user", "assistant", "system", "attachment"):
        return {"type": t}
    keep = ("type", "subtype", "uuid", "parentUuid", "logicalParentUuid", "isSidechain",
            "sessionId", "timestamp", "version", "isMeta", "isApiErrorMessage",
            "durationMs", "messageCount")
    out = {k: r[k] for k in keep if k in r}
    msg = r.get("message")
    if t in ("user", "assistant") and isinstance(msg, dict):
        m = {k: msg[k] for k in ("id", "role", "type", "model", "stop_reason") if k in msg}
        content = msg.get("content")
        if isinstance(content, str):
            m["content"] = content
        else:
            m["content"] = [claude_block(b) for b in content or []]
        out["message"] = m
    return out


def claude_block(b):
    t = b.get("type")
    if t == "text":
        return {"type": "text", "text": b.get("text", "")}
    if t == "thinking":
        return {"type": "thinking", "thinking": "", "signature": ""}
    if t == "tool_use":
        return {"type": "tool_use", "id": b.get("id"), "name": b.get("name"), "input": {}}
    if t == "tool_result":
        return {"type": "tool_result", "tool_use_id": b.get("tool_use_id"), "content": REDACTED}
    return {"type": t}


def codex_record(r):
    p = r.get("payload")
    t = r.get("type")
    out = {k: r[k] for k in ("timestamp", "ordinal", "type") if k in r}
    if not isinstance(p, dict):
        return out
    if t == "session_meta":
        keep = ("id", "session_id", "forked_from_id", "timestamp", "cwd", "originator",
                "cli_version", "source", "thread_source", "history_mode", "history_base")
        out["payload"] = {k: p[k] for k in keep if k in p}
    elif t == "event_msg":
        et = p.get("type")
        if et == "task_started":
            out["payload"] = pick(p, "type", "turn_id", "root_turn_id", "started_at")
        elif et == "task_complete":
            out["payload"] = pick(p, "type", "turn_id", "last_agent_message", "duration_ms")
        elif et == "turn_aborted":
            out["payload"] = pick(p, "type", "turn_id", "reason")
        elif et == "item_completed":
            item = p.get("item") or {}
            kept = pick(item, "type", "id", "phase")
            if item.get("type") == "AgentMessage":
                kept["content"] = [pick(c, "type", "text") for c in item.get("content", [])]
            out["payload"] = {**pick(p, "type", "thread_id", "turn_id"), "item": kept}
        else:
            out["payload"] = {"type": et}
    elif t == "response_item":
        rt = p.get("type")
        if rt == "message":
            m = pick(p, "type", "id", "role", "phase")
            m["content"] = [codex_content(p.get("role"), c) for c in p.get("content", [])]
            meta = p.get("internal_chat_message_metadata_passthrough")
            if isinstance(meta, dict) and "turn_id" in meta:
                m["internal_chat_message_metadata_passthrough"] = {"turn_id": meta["turn_id"]}
            out["payload"] = m
        elif rt in ("function_call", "custom_tool_call"):
            out["payload"] = pick(p, "type", "call_id", "name")
        elif rt in ("function_call_output", "custom_tool_call_output"):
            out["payload"] = {**pick(p, "type", "call_id"), "output": REDACTED}
        else:
            out["payload"] = pick(p, "type", "id")
    elif t == "turn_context":
        out["payload"] = pick(p, "turn_id")
    else:
        out["payload"] = {}
    return out


def codex_content(role, c):
    text = c.get("text", "")
    # Developer messages and environment context carry instructions and paths.
    if role != "assistant" and (role == "developer" or text.lstrip().startswith("<")):
        text = REDACTED
    return {"type": c.get("type"), "text": text}


def pick(d, *keys):
    return {k: d[k] for k in keys if k in d}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("kind", choices=("claude", "codex"))
    ap.add_argument("src")
    ap.add_argument("dest")
    ap.add_argument("--replace", action="append", default=[])
    args = ap.parse_args()
    pairs = [r.split("=", 1) for r in args.replace]
    convert = claude_record if args.kind == "claude" else codex_record
    with open(args.src, encoding="utf-8") as src, open(args.dest, "w", encoding="utf-8") as dest:
        for line in src:
            if not line.strip():
                continue
            text = json.dumps(convert(json.loads(line)), ensure_ascii=False)
            for old, new in pairs:
                text = text.replace(old, new)
            dest.write(text + "\n")


if __name__ == "__main__":
    main()
