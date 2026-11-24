---
description: Inspect, publish, branch-diff, and settle Omnigraph atoms alongside cross-service system graph status
argument-hint: "[status|publish|diff|keep] [args]"
---

Run an Omnigraph sync workflow for `$ARGUMENTS` using the `omnigraph-sync` protocol.

1. Always pass explicit `"project"` pointing to the target repository root on every MCP tool call.
2. Inspect branch & store readiness via `omnigraph_status` (and `system_graph_status` for multi-service workspaces).
3. Browse or search existing atoms with `omnigraph_atoms`, publish verified findings/decisions with `omnigraph_publish`, compare branches via `omnigraph_branches_diff`, and resolve divergent updates via `omnigraph_merge_keep`.
4. Use `orch_permission_mode` when a running session needs `auto`, `manual`, `bypass`, or rule revocation.
