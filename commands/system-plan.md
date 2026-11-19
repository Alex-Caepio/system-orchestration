---
description: Plan multi-service system changes using system_catalog, system_locate, system_impact, and system_plan
argument-hint: "[change-id] <task description>"
---

Plan a multi-service change for `$ARGUMENTS`.

1. Identify the target repository root directory and pass it explicitly as `"root"` / `"project"` on every MCP tool call.
2. Inspect `system.json` via `system_catalog` (or initialize a new workspace with `system_init` if requested).
3. Use `system_locate` and `system_impact` to trace affected services and downstream contract consumers.
4. Generate a bounded plan with `system_plan` (`context_budget: 8000`) and summarize the steps, warnings, and impacted services for review before executing.
