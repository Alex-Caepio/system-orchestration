---
name: system-run
description: Execute, monitor, and resume multi-service orchestration runs with sequential sandboxed workers, durable checkpoints, and live SQLite RunWorkingMemory.
---

# System Run

Use this skill to execute a reviewed `plan.json` across services, monitor run status, inspect SQLite working memory, or recover an interrupted run.

> **Codex & MCP note**: Always pass explicit `"project"` / `"root"` pointing to the target workspace root on every MCP tool call.

## Workflow

1. **Execute a reviewed plan**:
   - Call `system_execute` with `{"root": "<workspace-root>", "plan": "<plan.json>", "run_dir": "<run-dir>", "provider": "claude|codex|cursor", "mode": "read-only|edit", "access": "service|all", "working_memory": true}`.
2. **Inspect shared RunWorkingMemory**:
   - Call `orch_working_memory_read` with `{"project": "<workspace-root>", "run_dir": "<run-dir>", "scope_id": "<run_id>"}` to verify structured facts, decisions, questions, and hypotheses persisted between sequential service workers.
   - Use `orch_working_memory_write` to add curated cross-step notes (`fact`, `decision`, `open_question`, `hypothesis`, `progress`) protected by the privacy secret gate.
3. **Monitor or resume**:
   - Call `system_run_status` with `{"root": "<workspace-root>", "run_dir": "<run-dir>"}` to inspect completed receipts or detect interrupted runs.
   - If interrupted or failed after fixing a shared contract, call `system_resume` with `retry_step` and `accept_source_changes: true` as needed.
4. **Verify results locally**:
   - Execution never commits or pushes changes automatically; use `orch_delivery_preview` before staging local commits.
