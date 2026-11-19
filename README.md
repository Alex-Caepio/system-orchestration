# system-orchestration

Standalone, zero-dependency Rust workspace + Claude Code & Codex CLI dual plugin for:

- **Multi-Service System Orchestration (`orch-system`)**: `system.json` / `ai-service.json` schema validation, transitive contract impact graphs, deterministic bounded planning, sequential sandboxed worker execution, checkpoint recovery, and live SQLite `RunWorkingMemory` (`run_notes` with FTS/BM25-free typed scopes, TTL, and secret-privacy gates).
- **Session Scheduler & Atomic Inbox (`orch-harness`)**: bounded concurrent session slots, mount/worktree lease verification, atomic SQLite WAL inbox queueing with idempotency keys, cross-session bounded `context_draft` handoffs, multi-lens `Fleet` code review, adversarial two-provider `Clash` debates, SDD phase tracking, and local Git `Delivery` commit/cherry-pick previews.
- **Standalone Attached Overlay & Dispatch Hooks**: portable CLI overlay injection (`attach`) for `claude`, `codex`, and `cursor`, plus subagent write-lock serialization (`PreToolUse`) and `.orch/messages/` dispatch relays (`PostToolUse` / `SubagentStop`).
- **Embedded WebAssembly Workbench (`orch-ui` + `web/`)**: standalone `wasm32-unknown-unknown` workbench (Sessions, System Orchestration, Results & Delivery, Run Activity) compiled into `bin/orch-engine`.

## Binaries & Workspace Crates

- `crates/orch-core` — filesystem/Git/custody (`PublicationFiles`), strict JSON, UTC clock, privacy secret detector, delegation capsule validator, local `.orch/messages/` dispatch channel
- `crates/orch-host` — portable process management, Linux/macOS/Windows sandbox helpers, CLI provider discovery (`claude`, `codex`, `cursor`), model catalog, delegation & rollout tracking
- `crates/orch-system` — multi-service system planner, executor, runner recovery, `LocalTaskJournal`, and SQLite `RunWorkingMemory` + `WorkingMemoryStore`
- `crates/orch-harness` — session store, scheduler, inbox, fleet, clash, SDD, delivery, attachments, `chat_merge`, Axum HTTP API, dispatch hooks, and attached overlay
- `crates/orch-ui` — WebAssembly browser UI (`wasm-bindgen` `--target web`)
- `crates/orch-cli` — `bin/orch-engine` CLI, stdio JSON-RPC 2.0 MCP server (`orch-engine mcp`), and embedded loopback web server (`orch-engine serve`)

## Installation

Via the marketplace ([`Alex-Caepio/ai-infra-plugins`](https://github.com/Alex-Caepio/ai-infra-plugins)):

```bash
# Claude Code
claude plugin marketplace add Alex-Caepio/ai-infra-plugins
claude plugin install system-orchestration@ai-infra-plugins

# Codex CLI
codex plugin marketplace add https://github.com/Alex-Caepio/ai-infra-plugins.git
codex plugin add system-orchestration@ai-infra-plugins
```

Standalone repository: <https://github.com/Alex-Caepio/system-orchestration>

## Quick Start

### CLI

```bash
./bin/orch-engine --help
./bin/orch-engine system init --root /path/to/my-system --name "Platform"
./bin/orch-engine system catalog --system /path/to/my-system/system.json
./bin/orch-engine fleet run --project /path/to/repo --worker dry-run
./bin/orch-engine serve --project /path/to/repo --port 8766
```

### Stdio MCP (`18` tools)

Start via `orch-engine mcp`. Always pass explicit `"project"` / `"root"` pointing to your target repository root on every MCP tool call (required when running under Codex where server `cwd` is the plugin cache directory):

- `system_init`, `system_catalog`, `system_locate`, `system_impact`, `system_plan`, `system_execute`, `system_resume`, `system_run_status`
- `orch_session_start`, `orch_session_send`, `orch_session_status`, `orch_session_cancel`, `orch_context_draft`
- `orch_fleet_review`, `orch_clash_debate`, `orch_delivery_preview`
- `orch_working_memory_read`, `orch_working_memory_write`

## Dual Plugin Layout

- `.claude-plugin/plugin.json`, `.mcp.json`, `hooks/hooks.json` — Claude Code plugin
- `.codex-plugin/plugin.json`, `.codex-mcp.json` (`"cwd": "."`), `hooks/codex-hooks.json` — Codex CLI plugin
- `skills/{system-plan,system-run,multi-agent-dispatch,clash-debate,fleet-review}/SKILL.md`
- `commands/{system-plan,system-run,fleet-review,clash,orch-ui}.md`

## Build From Source

```bash
CARGO_TARGET_DIR=/tmp/orch-build \
  CARGO_INCREMENTAL=0 \
  cargo test --workspace --offline --locked

# Optional WASM workbench rebuild (`crates/orch-ui` -> `web/assets/`)
CARGO_TARGET_DIR=/tmp/orch-build \
  cargo build -p orch-ui --target wasm32-unknown-unknown --release --offline --locked
wasm-bindgen --target web --no-typescript --out-dir web/assets \
  /tmp/orch-build/wasm32-unknown-unknown/release/orch_ui.wasm

# Build release binary
CARGO_TARGET_DIR=/tmp/orch-build \
  CARGO_INCREMENTAL=0 \
  cargo build --release --offline --locked --bin orch-engine
strip /tmp/orch-build/release/orch-engine
cp /tmp/orch-build/release/orch-engine ./bin/orch-engine
```
