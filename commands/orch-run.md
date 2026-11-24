---
description: Execute or resume a reviewed multi-service plan with live SQLite RunWorkingMemory
argument-hint: "<plan.json> [--run-dir <dir>] [--mode read-only|edit]"
---

Execute or resume the multi-service orchestration plan specified by `$ARGUMENTS`.

1. Always pass `"root"` / `"project"` pointing to the target repository root on MCP calls.
2. Call `system_execute` with `"working_memory": true` (default) and the reviewed `plan` and `run_dir`, or `system_resume` if recovering an interrupted run.
3. Read structured cross-step state via `orch_working_memory_read` (`scope_id` set to the `run_id`) and inspect `system_run_status` receipts before previewing commits with `orch_delivery_preview`.
