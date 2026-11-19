---
name: clash-debate
description: Run an adversarial multi-round challenger debate between two independent providers to stress-test code changes or review findings before converging on a verdict.
---

# Clash Debate

Use this skill when a critical change or high-stakes code review benefits from an adversarial two-party protocol (`implementer` vs read-only `challenger`).

> **Codex & MCP note**: Always pass explicit `"project"` pointing to the target repository root on every MCP tool call.

## Protocol

1. **Start a clash debate**:
   - Call `orch_clash_debate` with:
     - `project`: `<workspace-root>`
     - `prompt`: detailed change or review objective
     - `provider`: primary participant (`claude`, `codex`, or `cursor`)
     - `challenger`: distinct challenger provider (different from primary, e.g., `codex` vs `claude`)
     - `workflow`: `native` (implement + challenge) or `review` (review + dispute findings)
     - `rounds`: `1..=3` (default `2`)
2. **Track convergence**:
   - Poll with `orch_clash_debate` passing `{"project": "<workspace-root>", "session_id": "<id>"}` (or `orch_session_status`) to inspect challenge objections, rebuttals, unresolved disputes, and the final verdict.
3. **Preview verified changes**:
   - Once converged, run `orch_delivery_preview` on the `session_id` to inspect changed files before committing locally.
