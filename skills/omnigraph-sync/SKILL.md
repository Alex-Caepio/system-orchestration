---
name: omnigraph-sync
description: Inspect, publish, diff, search, and settle Omnigraph context atoms, cross-service system graph bridges, and session permission modes without losing branch isolation.
---

# Omnigraph Sync

Use this skill when synchronizing findings or decisions into Omnigraph, comparing session/feature Omnigraph branches before merging into `main`, resolving divergent updates cleanly, or checking cross-service `system_graph` readiness.

> **Codex & MCP note**: Always pass explicit `"project"` pointing to the target repository root on every MCP tool call so tools never inspect the plugin directory itself.

## Core Tools

1. **Inspect graph & branch health**:
   - `omnigraph_status` (`project`, optional `branch`) — inspect connection mode (`off` when unconfigured, `connected` when running local/Docker/remote Omnigraph), branch counts, and stats.
   - `system_graph_status` (`project`, optional `config_path`, `check`, `service`) — inspect cross-service `system.json` code-graph & semantic bridge readiness.
2. **Search, filter, and publish atoms**:
   - `omnigraph_atoms` (`project`, optional `branch`, `kind`, `query`/`q`, `session_id`, `limit`, `offset`, `cursor`, `search`) — browse overlay graph edges, architecture violations, or run scored search (`"search": true`).
   - `omnigraph_publish` (`project`, required `title`; optional `kind`, `summary`, `body`, `branch`, `atom_id`, `session_id`, `tags`, `files`, `confidence`, `pin_priority`, `expected_commit`) — publish verified findings, decisions, or insights.
3. **Compare and settle branch divergence**:
   - `omnigraph_branches_diff` (`project`, required `source`, optional `target` default `"main"`) — inspect added/updated/deleted atoms, edge deltas, and `conflicts`.
   - `omnigraph_merge_keep` (`project`, required `atom_id`, `from`, optional `to` default `"main"`, `expected_updated_at`, `replaced_updated_at`) — settle a divergent atom by keeping the chosen branch version onto the target branch without erasing source provenance.
4. **Live session permissions**:
   - `orch_permission_mode` (`project`, required `session_id`; optional `mode`: `"auto" | "manual" | "bypass" | "default"`, `request_id`+`decision`+`answers`, or `revoke_rule_id`/`revoke_rule_key`/`revoke_all`) — adjust session approval autonomy or answer/revoke permissions safely.

## Workflow Checklist

1. Pass explicit `"project"` targeting the user's git repository.
2. Call `omnigraph_status` (and `system_graph_status` when working across multiple services) before publishing or merging.
3. Prefer deterministic `atom_id` slugs and attach relevant repo-relative `files` so Omnigraph overlays architectural violations accurately.
4. Before merging a session branch (`session/<sid>`), run `omnigraph_branches_diff`; resolve any `divergent_update` conflicts explicitly with `omnigraph_merge_keep`.
5. Never embed secrets, credentials, or raw tokens in `title`, `summary`, `body`, or `tags`.
