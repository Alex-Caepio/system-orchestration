---
name: multi-agent-dispatch
description: Orchestrate isolated agent sessions, atomic follow-up inbox queues, cross-session bounded context drafts, write-lock subagent gating, and local dispatch channels.
---

# Multi-Agent Dispatch

Use this skill when coordinating long-running or multi-turn agent sessions, queueing follow-up turns atomically, passing bounded handoff drafts across chats, or inspecting local delivery state.

> **Codex & MCP note**: Always pass explicit `"project"` pointing to the target workspace root on every MCP tool call.

## Capabilities

- **Session lifecycle**:
  - Start an isolated session with `orch_session_start` (`workflow`: `native`, `plan`, `review`, `sdd`, or `fleet-review`; `workspace`: `project`, `worktree`, `existing-worktree`).
  - Poll events and inbox receipts via `orch_session_status`.
  - Send follow-up prompts cleanly with `orch_session_send` (automatically routes through the atomic inbox queue with optional `request_id` when active).
  - Abort active runs cleanly with `orch_session_cancel`.
- **Cross-session handoffs**:
  - Extract a bounded summary from a completed session using `orch_context_draft` without copying poisoned context or exceeding token budgets.
- **Working memory & local delivery**:
  - Persist durable scoped facts and decisions via `orch_working_memory_write` / `orch_working_memory_read`.
  - Preview unpushed git changes, local commits, or branch cherry-pick transfers via `orch_delivery_preview`.
- **Write serialization hook**:
  - `orch-engine hook subagent-gate` / `subagent-dispatch` automatically serializes write-capable subagents via flock locks and relays local `.orch/messages/` receipts.
