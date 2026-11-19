---
name: system-plan
description: Plan multi-service cross-repository changes by validating system.json catalogs, locating affected services, computing downstream contract impact, and generating a bounded execution plan.
---

# System Plan

Use this skill to design and verify a multi-service change plan before launching any worker execution.

> **Codex & MCP note**: Always pass explicit `"project"` (or `"root"`) pointing to the target workspace root on every MCP tool call so tools never inspect the plugin directory itself.

## Workflow

1. **Inspect or initialize catalog**:
   - Call `system_catalog` with `{"root": "<workspace-root>"}` (and optional `allow_root` for external service directories).
   - If `system.json` does not yet exist, call `system_init` on a clean new directory or inspect CLI `orch-engine system init --root <dir> --name <name>`.
2. **Locate affected services**:
   - Call `system_locate` with `{"root": "<workspace-root>", "task": "<change description>"}`.
3. **Trace contract impact**:
   - Call `system_impact` with seed `service` IDs and/or changed `contract` identifiers and `impact_depth` (`0..=64`).
4. **Build and save the plan**:
   - Call `system_plan` with `{"root": "<workspace-root>", "task": "<goal>", "change_id": "<slug>", "service": [...], "contract": [...], "context_budget": 8000, "output": "<plan.json>"}`.
5. **Human checkpoint**:
   - Present selected services, downstream consumers, bounded context size, source warnings, and any `allow_root` requirements before running `system-run`.
