---
name: fleet-review
description: Dispatch parallel multi-lens fleet reviewers across a branch or working tree, deduplicate findings, and gate final reports behind explicit human approval.
---

# Fleet Review

Use this skill to run bounded multi-lens parallel code reviews (`code-reviewer`, `security-reviewer`, `performance-optimization`, or custom lenses) with SQLite checkpoints and an approval gate.

> **Codex & MCP note**: Always pass explicit `"project"` pointing to the target repository root on every MCP tool call.

## Workflow

1. **Launch a fleet review**:
   - Call `orch_fleet_review` with:
     - `project`: `<workspace-root>`
     - `worker`: `"claude"`, `"codex"`, `"cursor"`, or `"dry-run"` (offline verification)
     - `lenses`: array of distinct reviewer lens IDs (1–40)
     - `scope`: target diff scope (e.g., `"changes against main"`)
     - Optional `model`, `budget_usd`, `worker_timeout`
2. **Human approval gate**:
   - When the run pauses at `"awaiting_approval"`, inspect the findings ledger and report preview.
   - Resume explicitly via `orch_fleet_review` with `{"project": "<workspace-root>", "thread": "<thread-id>", "resume": true, "approve": true}` (or `"reject": true`).
3. **CLI alternative**:
   - `orch-engine fleet run --project <path> --worker claude-cli`
   - `orch-engine fleet resume --project <path> --thread <id> --approve`
