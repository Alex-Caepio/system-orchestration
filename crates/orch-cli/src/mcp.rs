use orch_core::{Result, RuntimeError};
use orch_harness::{
    blackboard::LocalBlackboard,
    desktop::{ServerConfig, ServerControl, ServicesFactory},
    native::NativeFactory,
    services::{Payload, Services},
    store::SessionStore,
};
use orch_system::{catalog::System, working_memory::notes::WorkingMemoryStore};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

const PROTOCOL_VERSION: &str = "2025-06-18";

struct RuntimeCache {
    runtimes: tokio::sync::Mutex<BTreeMap<PathBuf, Arc<dyn Services>>>,
}

impl RuntimeCache {
    fn new() -> Self {
        Self {
            runtimes: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    async fn for_state(&self, state: &Path, project: &Path) -> Result<Arc<dyn Services>> {
        let mut guard = self.runtimes.lock().await;
        if let Some(services) = guard.get(state) {
            return Ok(services.clone());
        }
        let (tx, _) = tokio::sync::watch::channel(false);
        let config = ServerConfig {
            source: project.to_path_buf(),
            state: state.to_path_buf(),
            port: 0,
            timeout: 900,
            session_concurrency: 2,
            projects: vec![project.to_path_buf()],
            providers: BTreeMap::new(),
            path_env: std::env::var_os("PATH"),
        };
        let control = Arc::new(ServerControl {
            config: config.clone(),
            signal: tx,
        });
        let services = NativeFactory
            .create(&config, control)
            .await
            .map_err(|e| RuntimeError::unavailable(e.message))?;
        guard.insert(state.to_path_buf(), services.clone());
        Ok(services)
    }

    async fn close_all(&self) {
        let mut guard = self.runtimes.lock().await;
        for (_, services) in std::mem::take(&mut *guard) {
            let _ = services.close().await;
        }
    }
}

pub fn is_plugin_dir(path: &Path) -> bool {
    path.join(".codex-plugin").is_dir()
        || path.join(".claude-plugin").is_dir()
        || (path.join("bin/orch-engine").is_file() && !path.join("system.json").is_file())
        || path
            .to_string_lossy()
            .replace('\\', "/")
            .contains(".codex/plugins/cache")
}

/// Resolve `project` / `root` cleanly.
/// Checks both omitted and relative (`"."`) inputs against `is_plugin_dir` BEFORE
/// touching any ancestor `.git` directory such as `~/.git` or a marketplace checkout.
pub fn resolve_project_root(args: &Value) -> Result<PathBuf> {
    let explicit = args
        .get("project")
        .or_else(|| args.get("root"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(raw) = explicit {
        let candidate = Path::new(raw);
        let path = orch_core::fs::canonicalize(candidate)?;
        if !path.is_dir() {
            return Err(RuntimeError::not_found(format!(
                "Repository path is not a directory: {}",
                path.display()
            )));
        }
        if is_plugin_dir(&path) {
            return Err(RuntimeError::policy(
                "Target path resolves to a plugin directory; pass explicit `project` or `root` pointing to the target repository.",
            ));
        }
        return Ok(path);
    }
    let cwd = std::env::current_dir()?;
    if is_plugin_dir(&cwd) {
        return Err(RuntimeError::policy(
            "Current working directory is inside a plugin directory; pass explicit `project` or `root` pointing to the target repository.",
        ));
    }
    let path = orch_core::fs::canonicalize(&cwd)?;
    if is_plugin_dir(&path) {
        return Err(RuntimeError::policy(
            "Current working directory is inside a plugin directory; pass explicit `project` or `root` pointing to the target repository.",
        ));
    }
    Ok(path)
}

/// Resolve the target directory for `system_init` (which creates a brand-new folder).
fn resolve_init_root(args: &Value) -> Result<PathBuf> {
    let explicit = args
        .get("project")
        .or_else(|| args.get("root"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(raw) = explicit else {
        return Err(RuntimeError::invalid(
            "system_init requires an explicit non-existent `root` path.",
        ));
    };
    let candidate = PathBuf::from(raw);
    let abs = if candidate.is_absolute() {
        candidate
    } else {
        let cwd = std::env::current_dir()?;
        if is_plugin_dir(&cwd) {
            return Err(RuntimeError::policy(
                "Refusing relative `root` while running inside a plugin directory; pass an absolute target `root`.",
            ));
        }
        cwd.join(candidate)
    };
    if let Some(parent) = abs.parent()
        && is_plugin_dir(parent)
    {
        return Err(RuntimeError::policy(
            "Refusing to initialize inside a plugin directory.",
        ));
    }
    Ok(abs)
}

fn resolve_state_dir(project: &Path, args: &Value) -> Result<PathBuf> {
    if let Some(raw) = args
        .get("state_dir")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let p = PathBuf::from(raw);
        return Ok(if p.is_absolute() { p } else { project.join(p) });
    }
    Ok(project.join(".orch").join("state"))
}

fn resolve_subpath(base: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() { p } else { base.join(p) }
}

fn system_path(project: &Path, args: &Value) -> PathBuf {
    let raw = args
        .get("system")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("system.json");
    resolve_subpath(project, raw)
}

fn allow_roots(project: &Path, args: &Value) -> Result<Vec<PathBuf>> {
    let Some(items) = args.get("allow_root").and_then(Value::as_array) else {
        return Ok(vec![]);
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let s = item
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RuntimeError::invalid("allow_root entries must be non-empty paths"))?;
        out.push(resolve_subpath(project, s));
    }
    Ok(out)
}

fn unwrap_system_reply(value: Value) -> Result<Value> {
    if let Some(cli) = value.get("_cli") {
        let code = cli["code"].as_i64().unwrap_or(1);
        let stdout = cli["stdout"].as_str().unwrap_or("").trim();
        let stderr = cli["stderr"].as_str().unwrap_or("").trim();
        if code != 0 {
            let parsed = if !stdout.is_empty() {
                serde_json::from_str::<Value>(stdout).ok()
            } else {
                None
            };
            if let Some(v) = parsed {
                return Ok(v);
            }
            return Err(RuntimeError::conflict(if !stderr.is_empty() {
                stderr.to_string()
            } else {
                stdout.to_string()
            }));
        }
        if !stdout.is_empty() {
            if let Ok(parsed) = serde_json::from_str::<Value>(stdout) {
                return Ok(parsed);
            }
            return Ok(json!({ "output": stdout }));
        }
        return Ok(json!({ "ok": true }));
    }
    Ok(value)
}

async fn ensure_project(services: &dyn Services, project: &Path) -> Result<String> {
    let canonical = orch_core::fs::canonicalize(project)?;
    let list = match services
        .request("projects.list", json!({ "params": {}, "query": {}, "body": {} }))
        .await
        .map_err(|e| RuntimeError::unavailable(e.message))?
    {
        Payload::Json(v) => v,
        _ => json!({}),
    };
    for item in list["projects"].as_array().into_iter().flatten() {
        if let Some(p) = item["path"].as_str()
            && Path::new(p) == canonical
            && let Some(id) = item["id"].as_str()
        {
            return Ok(id.to_string());
        }
    }
    let added = match services
        .request(
            "projects.add",
            json!({
                "params": {},
                "query": {},
                "body": { "path": canonical.to_string_lossy() }
            }),
        )
        .await
        .map_err(|e| RuntimeError::invalid(e.message))?
    {
        Payload::Json(v) => v,
        _ => json!({}),
    };
    added["project"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| RuntimeError::unavailable("Registered project did not return an ID."))
}

fn payload_json(payload: Payload) -> Result<Value> {
    match payload {
        Payload::Json(v) => Ok(v),
        Payload::Bytes { bytes, .. } => Ok(json!({ "bytes": bytes.len() })),
    }
}

fn string_list(args: &Value, key: &str) -> Result<Vec<String>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(vec![s.trim().to_string()]),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| RuntimeError::invalid(format!("Expected non-empty strings in `{key}`")))
            })
            .collect(),
        _ => Err(RuntimeError::invalid(format!("Invalid `{key}` list"))),
    }
}

pub fn tools_list() -> Value {
    let pr = || {
        json!({
            "project": { "type": "string", "description": "Target repository root directory (strongly recommended; required if server cwd is a plugin directory)." },
            "root": { "type": "string", "description": "Alias for `project`." }
        })
    };
    let with_pr = |mut props: Map<String, Value>| -> Value {
        if let Some(base) = pr().as_object() {
            for (k, v) in base {
                props.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        Value::Object(props)
    };
    let schema = |props: Value, required: &[&str]| {
        json!({
            "type": "object",
            "properties": with_pr(props.as_object().cloned().unwrap_or_default()),
            "required": required,
            "additionalProperties": false
        })
    };
    json!({
        "tools": [
            {
                "name": "system_init",
                "description": "Initialize a new multi-service system workspace (system.json, AGENTS.md, README.md) at a target directory.",
                "inputSchema": schema(json!({
                    "name": { "type": "string", "description": "Human-readable system name (defaults to directory name)." }
                }), &[])
            },
            {
                "name": "system_catalog",
                "description": "Validate and return the system service catalog, contracts, relationships, and warnings from system.json.",
                "inputSchema": schema(json!({
                    "system": { "type": "string", "description": "Path to system.json (default: system.json relative to root)." },
                    "allow_root": { "type": "array", "items": { "type": "string" } }
                }), &[])
            },
            {
                "name": "system_locate",
                "description": "Locate candidate services, contracts, and source files matching a change task description.",
                "inputSchema": schema(json!({
                    "task": { "type": "string", "description": "Natural language task description." },
                    "system": { "type": "string" },
                    "allow_root": { "type": "array", "items": { "type": "string" } }
                }), &["task"])
            },
            {
                "name": "system_impact",
                "description": "Compute transitive downstream contract impact across system services.",
                "inputSchema": schema(json!({
                    "service": { "type": "array", "items": { "type": "string" }, "description": "Seed service IDs." },
                    "contract": { "type": "array", "items": { "type": "string" }, "description": "Changed contract identifiers." },
                    "impact_depth": { "type": "integer", "minimum": 0, "maximum": 64 },
                    "system": { "type": "string" },
                    "allow_root": { "type": "array", "items": { "type": "string" } }
                }), &[])
            },
            {
                "name": "system_plan",
                "description": "Generate and validate a deterministic multi-service execution plan with bounded context.",
                "inputSchema": schema(json!({
                    "task": { "type": "string" },
                    "change_id": { "type": "string" },
                    "service": { "type": "array", "items": { "type": "string" } },
                    "contract": { "type": "array", "items": { "type": "string" } },
                    "context_budget": { "type": "integer", "minimum": 0, "maximum": 64000 },
                    "impact_depth": { "type": "integer", "minimum": 0, "maximum": 64 },
                    "output": { "type": "string", "description": "Optional path to write the plan JSON artifact." },
                    "system": { "type": "string" },
                    "allow_root": { "type": "array", "items": { "type": "string" } }
                }), &["task", "change_id"])
            },
            {
                "name": "system_execute",
                "description": "Execute a reviewed system plan with sequential sandboxed service workers and live SQLite RunWorkingMemory.",
                "inputSchema": schema(json!({
                    "plan": { "type": "string", "description": "Path to plan JSON." },
                    "run_dir": { "type": "string", "description": "Directory for durable run journal, receipts, and working-memory SQLite state." },
                    "provider": { "type": "string", "enum": ["claude", "codex", "cursor"] },
                    "mode": { "type": "string", "enum": ["read-only", "edit"] },
                    "access": { "type": "string", "enum": ["all", "service"] },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 86400 },
                    "working_memory": { "type": "boolean", "description": "Enable live SQLite RunWorkingMemory (default: true)." },
                    "system": { "type": "string" },
                    "allow_root": { "type": "array", "items": { "type": "string" } }
                }), &["plan", "run_dir"])
            },
            {
                "name": "system_resume",
                "description": "Resume an interrupted or failed multi-service system run from its checkpoint journal.",
                "inputSchema": schema(json!({
                    "run_dir": { "type": "string" },
                    "retry_step": { "type": "string" },
                    "accept_source_changes": { "type": "boolean" },
                    "accept_unrun_checks": { "type": "boolean" },
                    "system": { "type": "string" },
                    "allow_root": { "type": "array", "items": { "type": "string" } }
                }), &["run_dir"])
            },
            {
                "name": "system_run_status",
                "description": "Inspect live or finished system run state, step receipts, and interrupted worker detection.",
                "inputSchema": schema(json!({
                    "run_dir": { "type": "string" },
                    "system": { "type": "string" },
                    "allow_root": { "type": "array", "items": { "type": "string" } }
                }), &["run_dir"])
            },
            {
                "name": "orch_session_start",
                "description": "Start a new managed workspace orchestration session (native, plan, review, sdd, or fleet-review).",
                "inputSchema": schema(json!({
                    "prompt": { "type": "string" },
                    "provider": { "type": "string", "enum": ["claude", "codex", "cursor"] },
                    "workflow": { "type": "string", "enum": ["native", "plan", "review", "sdd", "fleet-review"] },
                    "mode": { "type": "string", "enum": ["plan", "edit"] },
                    "workspace": { "type": "string", "enum": ["project", "worktree", "existing-worktree"] },
                    "worktree_branch": { "type": "string" },
                    "agents_enabled": { "type": "boolean" },
                    "agent_count": { "type": "integer", "minimum": 1, "maximum": 40 },
                    "permissions": { "type": "string", "enum": ["auto", "manual", "bypass"], "description": "Optional permission mode for edit sessions." },
                    "dry_run": { "type": "boolean" },
                    "state_dir": { "type": "string" }
                }), &["prompt"])
            },
            {
                "name": "orch_session_send",
                "description": "Send a follow-up prompt to an existing session or enqueue an atomic inbox follow-up on a running native session.",
                "inputSchema": schema(json!({
                    "session_id": { "type": "string" },
                    "prompt": { "type": "string" },
                    "request_id": { "type": "string", "description": "Optional client idempotency key for active-session inbox queueing." },
                    "mode": { "type": "string", "enum": ["plan", "edit"] },
                    "permissions": { "type": "string", "enum": ["auto", "manual", "bypass"] },
                    "state_dir": { "type": "string" }
                }), &["session_id", "prompt"])
            },
            {
                "name": "orch_session_status",
                "description": "Read a session's current state, events, inbox receipts, or list recent sessions when session_id is omitted.",
                "inputSchema": schema(json!({
                    "session_id": { "type": "string" },
                    "after": { "type": "integer", "minimum": 0 },
                    "state_dir": { "type": "string" }
                }), &[])
            },
            {
                "name": "orch_session_cancel",
                "description": "Cancel an active/queued orchestration session and settle any active inbox items safely.",
                "inputSchema": schema(json!({
                    "session_id": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &["session_id"])
            },
            {
                "name": "orch_context_draft",
                "description": "Generate a bounded cross-session task context draft from a completed session for handoff into another task.",
                "inputSchema": schema(json!({
                    "source_session_id": { "type": "string" },
                    "destination_session_id": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &["source_session_id"])
            },
            {
                "name": "orch_fleet_review",
                "description": "Run or resume a multi-lens parallel fleet code review (supports offline dry-run or claude/codex/cursor workers).",
                "inputSchema": schema(json!({
                    "thread": { "type": "string" },
                    "worker": { "type": "string", "enum": ["dry-run", "claude", "codex", "cursor"] },
                    "lenses": { "type": "array", "items": { "type": "string" } },
                    "scope": { "type": "string" },
                    "model": { "type": "string" },
                    "budget_usd": { "type": "number" },
                    "worker_timeout": { "type": "integer", "minimum": 1, "maximum": 86400 },
                    "resume": { "type": "boolean" },
                    "approve": { "type": "boolean" },
                    "reject": { "type": "boolean" },
                    "state_dir": { "type": "string" }
                }), &[])
            },
            {
                "name": "orch_clash_debate",
                "description": "Start an adversarial challenger debate session (implementer vs read-only challenger) or inspect a clash session ledger.",
                "inputSchema": schema(json!({
                    "prompt": { "type": "string" },
                    "provider": { "type": "string", "enum": ["claude", "codex", "cursor"] },
                    "challenger": { "type": "string", "enum": ["claude", "codex", "cursor"] },
                    "workflow": { "type": "string", "enum": ["native", "review"] },
                    "rounds": { "type": "integer", "minimum": 1, "maximum": 3 },
                    "session_id": { "type": "string", "description": "Inspect existing clash session ledger when provided." },
                    "state_dir": { "type": "string" }
                }), &[])
            },
            {
                "name": "orch_delivery_preview",
                "description": "Inspect git delivery state, preview a reviewed local commit, or preview cherry-picking a commit onto a target branch without pushing.",
                "inputSchema": schema(json!({
                    "session_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["status", "preview_commit", "preview_transfer"] },
                    "snapshot_id": { "type": "string" },
                    "paths": { "type": "array", "items": { "type": "string" } },
                    "message": { "type": "string" },
                    "commit": { "type": "string" },
                    "target_branch": { "type": "string" },
                    "check": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &["session_id"])
            },
            {
                "name": "orch_working_memory_read",
                "description": "Read active, non-expired structured working-memory notes (facts, decisions, questions, hypotheses) from SQLite run_notes.",
                "inputSchema": schema(json!({
                    "scope_id": { "type": "string", "description": "Session ID or run ID scope." },
                    "include_resolved": { "type": "boolean" },
                    "run_dir": { "type": "string", "description": "Optional run directory holding working-memory SQLite store." },
                    "state_dir": { "type": "string" }
                }), &["scope_id"])
            },
            {
                "name": "orch_working_memory_write",
                "description": "Write or update a structured, privacy-gated working-memory note into SQLite run_notes.",
                "inputSchema": schema(json!({
                    "scope_id": { "type": "string" },
                    "key": { "type": "string" },
                    "kind": { "type": "string", "enum": ["progress", "hypothesis", "open_question", "decision", "fact"] },
                    "status": { "type": "string", "enum": ["open", "resolved"] },
                    "text": { "type": "string" },
                    "sources": { "type": "array", "items": { "type": "string" } },
                    "ttl_seconds": { "type": "integer", "minimum": 60, "maximum": 604800 },
                    "run_dir": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &["scope_id", "key", "text"])
            },
            {
                "name": "omnigraph_status",
                "description": "Inspect Omnigraph branch/store availability, active branches, atom counts, and head commit state for the target project.",
                "inputSchema": schema(json!({
                    "branch": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &[])
            },
            {
                "name": "omnigraph_atoms",
                "description": "Query, filter, paginate, or search Omnigraph context atoms, overlay code edges, and violation markers.",
                "inputSchema": schema(json!({
                    "branch": { "type": "string" },
                    "snapshot": { "type": "string" },
                    "kind": { "type": "string" },
                    "query": { "type": "string", "description": "Filter/search text." },
                    "q": { "type": "string", "description": "Alias for query filter/search text." },
                    "session_id": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "cursor": { "type": "string" },
                    "search": { "type": "boolean", "description": "Use scored full-text search (omnigraph.search) when true." },
                    "state_dir": { "type": "string" }
                }), &[])
            },
            {
                "name": "omnigraph_publish",
                "description": "Publish a structured context atom (finding, decision, insight, or fact) to Omnigraph with optional linked files and provenance.",
                "inputSchema": schema(json!({
                    "title": { "type": "string" },
                    "kind": { "type": "string" },
                    "summary": { "type": "string" },
                    "body": { "type": "string" },
                    "branch": { "type": "string" },
                    "atom_id": { "type": "string" },
                    "session_id": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "files": { "type": "array", "items": { "type": "string" } },
                    "confidence": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                    "pin_priority": { "type": "integer" },
                    "expected_commit": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &["title"])
            },
            {
                "name": "omnigraph_branches_diff",
                "description": "Compare Omnigraph atoms, edges, and divergent updates between a source branch and target branch (default: main).",
                "inputSchema": schema(json!({
                    "source": { "type": "string" },
                    "target": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &["source"])
            },
            {
                "name": "omnigraph_merge_keep",
                "description": "Resolve an Omnigraph divergent branch atom by keeping one branch's version onto the target branch.",
                "inputSchema": schema(json!({
                    "atom_id": { "type": "string" },
                    "from": { "type": "string" },
                    "to": { "type": "string" },
                    "expected_updated_at": { "type": "string" },
                    "replaced_updated_at": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &["atom_id", "from"])
            },
            {
                "name": "system_graph_status",
                "description": "Inspect cross-service system graph readiness, service code graph states, and semantic/layer health for system.json.",
                "inputSchema": schema(json!({
                    "config_path": { "type": "string", "description": "Optional system config path (default: system.json)." },
                    "check": { "type": "boolean", "description": "Re-verify file/git staleness when true." },
                    "service": { "type": "string" },
                    "state_dir": { "type": "string" }
                }), &[])
            },
            {
                "name": "orch_permission_mode",
                "description": "Inspect or update a session's permission mode (auto|manual|bypass|default), respond to pending permission requests, or revoke remembered session rules.",
                "inputSchema": schema(json!({
                    "session_id": { "type": "string" },
                    "mode": { "type": "string", "enum": ["auto", "manual", "bypass", "default"] },
                    "request_id": { "type": "string" },
                    "decision": { "type": "string", "enum": ["allow", "allow_session", "deny"] },
                    "answers": { "type": "object" },
                    "revoke_rule_id": { "type": "string" },
                    "revoke_rule_key": { "type": "string" },
                    "revoke_all": { "type": "boolean" },
                    "state_dir": { "type": "string" }
                }), &["session_id"])
            }
        ]
    })
}

async fn dispatch_tool(cache: &RuntimeCache, name: &str, args: &Value) -> Result<Value> {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut noop = |_ev: Value| Ok(());
    if name == "system_init" {
        let init_root = resolve_init_root(args)?;
        let sys_name = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                init_root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "system".into());
        let req = json!({
            "root": init_root.to_string_lossy(),
            "name": sys_name
        });
        let reply = orch_system::execute_async("init", &req, cancel, &mut noop).await?;
        return unwrap_system_reply(reply);
    }
    let project = resolve_project_root(args)?;
    let state = resolve_state_dir(&project, args)?;
    match name {
        "system_catalog" => {
            let sys_file = system_path(&project, args);
            let allow = allow_roots(&project, args)?
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let req = json!({
                "system": sys_file.to_string_lossy(),
                "allow_root": allow
            });
            let reply = orch_system::execute_async("catalog", &req, cancel, &mut noop).await?;
            unwrap_system_reply(reply)
        }
        "system_locate" => {
            let task = args["task"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `task` string"))?;
            let sys_file = system_path(&project, args);
            let allow = allow_roots(&project, args)?
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let req = json!({
                "system": sys_file.to_string_lossy(),
                "task": task,
                "allow_root": allow
            });
            let reply = orch_system::execute_async("locate", &req, cancel, &mut noop).await?;
            unwrap_system_reply(reply)
        }
        "system_impact" => {
            let sys_file = system_path(&project, args);
            let allow = allow_roots(&project, args)?;
            let services = string_list(args, "service")?;
            let contracts = string_list(args, "contract")?;
            let depth = args
                .get("impact_depth")
                .and_then(Value::as_u64)
                .unwrap_or(32) as usize;
            let mut sys = System::new(&sys_file, &allow)?;
            let impacted = sys.impact(&services, &contracts, depth)?;
            Ok(json!({
                "system": sys.config_data["name"].clone(),
                "impacted": impacted,
                "warnings": sys.warnings
            }))
        }
        "system_plan" => {
            let task = args["task"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `task` string"))?;
            let change_id = args["change_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `change_id` string"))?;
            let sys_file = system_path(&project, args);
            let allow = allow_roots(&project, args)?
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let mut req = json!({
                "system": sys_file.to_string_lossy(),
                "task": task,
                "change_id": change_id,
                "service": string_list(args, "service")?,
                "contract": string_list(args, "contract")?,
                "allow_root": allow
            });
            if let Some(budget) = args.get("context_budget").and_then(Value::as_u64) {
                req["context_budget"] = json!(budget);
            }
            if let Some(depth) = args.get("impact_depth").and_then(Value::as_u64) {
                req["impact_depth"] = json!(depth);
            }
            if let Some(out) = args.get("output").and_then(Value::as_str) {
                req["output"] = json!(resolve_subpath(&project, out).to_string_lossy());
            }
            let reply = orch_system::execute_async("plan", &req, cancel, &mut noop).await?;
            unwrap_system_reply(reply)
        }
        "system_execute" => {
            let plan_raw = args["plan"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `plan` path"))?;
            let run_raw = args["run_dir"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `run_dir` path"))?;
            let sys_file = system_path(&project, args);
            let allow = allow_roots(&project, args)?
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let req = json!({
                "system": sys_file.to_string_lossy(),
                "plan": resolve_subpath(&project, plan_raw).to_string_lossy(),
                "run_dir": resolve_subpath(&project, run_raw).to_string_lossy(),
                "provider": args.get("provider").and_then(Value::as_str).unwrap_or("codex"),
                "mode": args.get("mode").and_then(Value::as_str).unwrap_or("read-only"),
                "access": args.get("access").and_then(Value::as_str).unwrap_or("service"),
                "timeout": args.get("timeout").and_then(Value::as_u64).unwrap_or(900),
                "working_memory": args.get("working_memory").and_then(Value::as_bool).unwrap_or(true),
                "allow_root": allow
            });
            let reply = orch_system::execute_async("execute", &req, cancel, &mut noop).await?;
            unwrap_system_reply(reply)
        }
        "system_resume" => {
            let run_raw = args["run_dir"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `run_dir` path"))?;
            let sys_file = system_path(&project, args);
            let allow = allow_roots(&project, args)?
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let mut req = json!({
                "system": sys_file.to_string_lossy(),
                "run_dir": resolve_subpath(&project, run_raw).to_string_lossy(),
                "accept_source_changes": args.get("accept_source_changes") == Some(&Value::Bool(true)),
                "accept_unrun_checks": args.get("accept_unrun_checks") == Some(&Value::Bool(true)),
                "allow_root": allow
            });
            if let Some(step) = args.get("retry_step").and_then(Value::as_str) {
                req["retry_step"] = json!(step);
            }
            let reply = orch_system::execute_async("resume", &req, cancel, &mut noop).await?;
            unwrap_system_reply(reply)
        }
        "system_run_status" => {
            let run_raw = args["run_dir"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `run_dir` path"))?;
            let sys_file = system_path(&project, args);
            let allow = allow_roots(&project, args)?
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let req = json!({
                "system": sys_file.to_string_lossy(),
                "run_dir": resolve_subpath(&project, run_raw).to_string_lossy(),
                "allow_root": allow
            });
            let reply = orch_system::execute_async("run-status", &req, cancel, &mut noop).await?;
            unwrap_system_reply(reply)
        }
        "orch_session_start" => {
            let services = cache.for_state(&state, &project).await?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let prompt = args["prompt"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `prompt`"))?;
            let provider = args
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("claude");
            let workflow = args
                .get("workflow")
                .and_then(Value::as_str)
                .unwrap_or("native");
            let mut body = json!({
                "project_id": project_id,
                "provider": provider,
                "workflow": workflow,
                "mode": args.get("mode").and_then(Value::as_str).unwrap_or("plan"),
                "workspace": args.get("workspace").and_then(Value::as_str).unwrap_or("project"),
                "prompt": prompt
            });
            if let Some(branch) = args.get("worktree_branch").and_then(Value::as_str) {
                body["worktree_branch"] = json!(branch);
            }
            if let Some(v) = args.get("agents_enabled").and_then(Value::as_bool) {
                body["agents_enabled"] = json!(v);
            }
            if let Some(v) = args.get("agent_count").and_then(Value::as_u64) {
                body["agent_count"] = json!(v);
            }
            if let Some(perms) = args.get("permissions").and_then(Value::as_str) {
                body["permissions"] = json!(perms);
            }
            if workflow == "fleet-review" {
                body["fleet"] = json!({
                    "dry_run": args.get("dry_run").and_then(Value::as_bool).unwrap_or(false)
                });
            }
            let reply = services
                .request(
                    "sessions.create",
                    json!({ "params": {}, "query": {}, "body": body }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "orch_session_send" => {
            let sid = args["session_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `session_id`"))?;
            let prompt = args["prompt"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `prompt`"))?;
            let services = cache.for_state(&state, &project).await?;
            let current = payload_json(
                services
                    .request(
                        "sessions.get",
                        json!({ "params": { "session_id": sid }, "query": {}, "body": {} }),
                    )
                    .await
                    .map_err(|e| RuntimeError::invalid(e.message))?,
            )?;
            let status = current["session"]["status"].as_str().unwrap_or("");
            if status == "running" || status == "queued" {
                let req_id = args
                    .get("request_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(orch_core::domain::uuid4);
                let reply = services
                    .request(
                        "sessions.inbox_accept",
                        json!({
                            "params": { "session_id": sid },
                            "query": {},
                            "body": { "request_id": req_id, "prompt": prompt }
                        }),
                    )
                    .await
                    .map_err(|e| RuntimeError::conflict(e.message))?;
                return payload_json(reply);
            }
            let mut body = json!({ "prompt": prompt });
            if let Some(m) = args.get("mode").and_then(Value::as_str) {
                body["mode"] = json!(m);
            }
            if let Some(perms) = args.get("permissions").and_then(Value::as_str) {
                body["permissions"] = json!(perms);
            }
            let reply = services
                .request(
                    "sessions.send",
                    json!({
                        "params": { "session_id": sid },
                        "query": {},
                        "body": body
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "orch_session_status" => {
            let services = cache.for_state(&state, &project).await?;
            if let Some(sid) = args
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                let mut query = Map::new();
                if let Some(after) = args.get("after").and_then(Value::as_u64) {
                    query.insert("after".into(), json!(after.to_string()));
                }
                let session = payload_json(
                    services
                        .request(
                            "sessions.get",
                            json!({ "params": { "session_id": sid }, "query": query, "body": {} }),
                        )
                        .await
                        .map_err(|e| RuntimeError::not_found(e.message))?,
                )?;
                let inbox = payload_json(
                    services
                        .request(
                            "sessions.inbox_list",
                            json!({ "params": { "session_id": sid }, "query": {}, "body": {} }),
                        )
                        .await
                        .map_err(|e| RuntimeError::invalid(e.message))?,
                )?;
                Ok(json!({ "session": session["session"], "events": session["events"], "inbox": inbox }))
            } else {
                payload_json(
                    services
                        .request(
                            "sessions.list",
                            json!({ "params": {}, "query": {}, "body": {} }),
                        )
                        .await
                        .map_err(|e| RuntimeError::unavailable(e.message))?,
                )
            }
        }
        "orch_session_cancel" => {
            let sid = args["session_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `session_id`"))?;
            let services = cache.for_state(&state, &project).await?;
            let reply = services
                .request(
                    "sessions.cancel",
                    json!({ "params": { "session_id": sid }, "query": {}, "body": {} }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "orch_context_draft" => {
            let source_sid = args["source_session_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `source_session_id`"))?;
            let services = cache.for_state(&state, &project).await?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let mut query = Map::new();
            query.insert("project_id".into(), json!(project_id));
            if let Some(dest) = args.get("destination_session_id").and_then(Value::as_str) {
                query.insert("destination_id".into(), json!(dest));
            }
            let reply = services
                .request(
                    "sessions.context_draft",
                    json!({ "params": { "session_id": source_sid }, "query": query, "body": {} }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "orch_fleet_review" => {
            let store = SessionStore::open(&state).map_err(|e| RuntimeError::unavailable(e.message))?;
            let board_root = state.join("blackboard");
            orch_core::fs::ensure_directory(&board_root, true)?;
            let board = Arc::new(LocalBlackboard {
                task_store: board_root.join("tasks.sqlite3"),
                state_root: board_root,
            });
            let worker = args
                .get("worker")
                .and_then(Value::as_str)
                .unwrap_or("dry-run");
            let (provider, dry) = match worker {
                "dry-run" => ("claude", true),
                "claude" | "codex" | "cursor" => (worker, false),
                _ => return Err(RuntimeError::invalid("Choose dry-run, claude, codex, or cursor")),
            };
            let executable = if dry {
                PathBuf::from("offline")
            } else {
                orch_harness::provider::discover(&BTreeMap::new())
                    .await
                    .map_err(|e| RuntimeError::unavailable(e.message))?
                    .into_iter()
                    .find(|p| p.id == provider && p.available)
                    .and_then(|p| p.executable)
                    .ok_or_else(|| {
                        RuntimeError::unavailable(format!("Provider CLI `{provider}` is unavailable."))
                    })?
            };
            let lenses = match args.get("lenses").and_then(Value::as_array) {
                Some(items) if !items.is_empty() => items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
                    .collect::<Vec<_>>(),
                _ => vec![
                    "code-reviewer".into(),
                    "security-reviewer".into(),
                    "performance-optimization".into(),
                ],
            };
            if lenses.len() > 40 || lenses.iter().collect::<BTreeSet<_>>().len() != lenses.len() {
                return Err(RuntimeError::invalid("Choose distinct bounded reviewer lenses (1..=40)"));
            }
            let thread = args
                .get("thread")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "fleet-review-{}",
                        chrono::Utc::now().format("%Y%m%d-%H%M%S")
                    )
                });
            let reports = state.join("reports");
            orch_core::fs::ensure_directory(&reports, true)?;
            let config = orch_harness::fleet::Config {
                project: project.clone(),
                provider: provider.into(),
                executable,
                lenses,
                model: args.get("model").and_then(Value::as_str).map(str::to_string),
                effort: None,
                budget: args.get("budget_usd").and_then(Value::as_f64),
                seconds: args.get("worker_timeout").and_then(Value::as_u64).unwrap_or(600),
                concurrency: 3,
                dry_run: dry,
                reports,
                adapter: None,
                runner_lock: Some(store.runner_lock()),
            };
            let engine = orch_harness::fleet::Fleet {
                store,
                blackboard: board,
            };
            let scope = args
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("the current branch's changes against the default branch");
            let task = format!("harness/{thread}");
            engine
                .initialize(
                    &thread,
                    scope,
                    &task,
                    &config,
                    Some(&state.join("checkpoints.sqlite")),
                    None,
                )
                .map_err(|e| RuntimeError::invalid(e.message))?;
            if args.get("resume") == Some(&Value::Bool(true)) {
                let approve = args.get("approve") == Some(&Value::Bool(true));
                let reject = args.get("reject") == Some(&Value::Bool(true));
                if approve == reject {
                    return Err(RuntimeError::invalid(
                        "Resuming fleet review requires exactly one of `approve: true` or `reject: true`.",
                    ));
                }
                engine
                    .decide(&thread, approve)
                    .map_err(|e| RuntimeError::invalid(e.message))?;
            }
            let result = engine
                .drive(&thread, &config, cancel, Arc::new(|_| Ok(())))
                .await
                .map_err(|e| RuntimeError::unavailable(e.message))?;
            Ok(json!({ "thread": thread, "result": result }))
        }
        "orch_clash_debate" => {
            let services = cache.for_state(&state, &project).await?;
            if let Some(sid) = args
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                let reply = services
                    .request(
                        "sessions.get",
                        json!({ "params": { "session_id": sid }, "query": {}, "body": {} }),
                    )
                    .await
                    .map_err(|e| RuntimeError::not_found(e.message))?;
                let payload = payload_json(reply)?;
                return Ok(json!({
                    "session_id": sid,
                    "clash": payload["session"]["clash"],
                    "status": payload["session"]["status"],
                    "events": payload["events"]
                }));
            }
            let prompt = args
                .get("prompt")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Provide `prompt` to start a clash debate or `session_id` to inspect one."))?;
            let provider = args
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("claude");
            let challenger = args
                .get("challenger")
                .and_then(Value::as_str)
                .unwrap_or(if provider == "claude" { "codex" } else { "claude" });
            let workflow = args
                .get("workflow")
                .and_then(Value::as_str)
                .unwrap_or("review");
            let rounds = args.get("rounds").and_then(Value::as_u64).unwrap_or(2);
            let clash = json!({
                "enabled": true,
                "challenger": challenger,
                "rounds": rounds
            });
            orch_harness::clash::validate(&clash, workflow, provider)
                .map_err(|e| RuntimeError::invalid(e.message))?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let reply = services
                .request(
                    "sessions.create",
                    json!({
                        "params": {},
                        "query": {},
                        "body": {
                            "project_id": project_id,
                            "provider": provider,
                            "workflow": workflow,
                            "mode": "plan",
                            "workspace": "project",
                            "prompt": prompt,
                            "clash": clash
                        }
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "orch_delivery_preview" => {
            let sid = args["session_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `session_id`"))?;
            let services = cache.for_state(&state, &project).await?;
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("status");
            match action {
                "status" => payload_json(
                    services
                        .request(
                            "delivery.get",
                            json!({ "params": { "session_id": sid }, "query": {}, "body": {} }),
                        )
                        .await
                        .map_err(|e| RuntimeError::invalid(e.message))?,
                ),
                "preview_commit" => {
                    let snapshot_id = args["snapshot_id"]
                        .as_str()
                        .ok_or_else(|| RuntimeError::invalid("Missing `snapshot_id` for preview_commit"))?;
                    let message = args["message"]
                        .as_str()
                        .ok_or_else(|| RuntimeError::invalid("Missing `message` for preview_commit"))?;
                    let paths = string_list(args, "paths")?;
                    payload_json(
                        services
                            .request(
                                "delivery.act",
                                json!({
                                    "params": { "session_id": sid },
                                    "query": {},
                                    "body": {
                                        "action": "preview_commit",
                                        "snapshot_id": snapshot_id,
                                        "paths": paths,
                                        "message": message
                                    }
                                }),
                            )
                            .await
                            .map_err(|e| RuntimeError::invalid(e.message))?,
                    )
                }
                "preview_transfer" => {
                    let commit = args["commit"]
                        .as_str()
                        .ok_or_else(|| RuntimeError::invalid("Missing `commit` for preview_transfer"))?;
                    let target_branch = args["target_branch"]
                        .as_str()
                        .ok_or_else(|| RuntimeError::invalid("Missing `target_branch` for preview_transfer"))?;
                    let mut body = json!({
                        "action": "preview_transfer",
                        "commit": commit,
                        "target_branch": target_branch
                    });
                    if let Some(check) = args.get("check").and_then(Value::as_str) {
                        body["check"] = json!(check);
                    }
                    payload_json(
                        services
                            .request(
                                "delivery.act",
                                json!({ "params": { "session_id": sid }, "query": {}, "body": body }),
                            )
                            .await
                            .map_err(|e| RuntimeError::invalid(e.message))?,
                    )
                }
                other => Err(RuntimeError::invalid(format!(
                    "Unsupported delivery preview action `{other}`; use status, preview_commit, or preview_transfer."
                ))),
            }
        }
        "orch_working_memory_read" => {
            let scope_id = args["scope_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `scope_id`"))?;
            let include_resolved = args.get("include_resolved") == Some(&Value::Bool(true));
            let wm_dir = match args.get("run_dir").and_then(Value::as_str) {
                Some(raw) if !raw.trim().is_empty() => resolve_subpath(&project, raw).join("working-memory"),
                _ => state.join("working-memory"),
            };
            let store = WorkingMemoryStore::open(&wm_dir)
                .map_err(|e| RuntimeError::unavailable(e.to_string()))?;
            store
                .read_notes(scope_id, include_resolved)
                .map_err(|e| RuntimeError::invalid(e.to_string()))
        }
        "orch_working_memory_write" => {
            let scope_id = args["scope_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `scope_id`"))?;
            let key = args["key"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `key`"))?;
            let text = args["text"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing `text`"))?;
            let kind = args.get("kind").and_then(Value::as_str).unwrap_or("fact");
            let status = args.get("status").and_then(Value::as_str).unwrap_or("open");
            let ttl_seconds = args.get("ttl_seconds").and_then(Value::as_u64).unwrap_or(3600);
            let sources = args
                .get("sources")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let wm_dir = match args.get("run_dir").and_then(Value::as_str) {
                Some(raw) if !raw.trim().is_empty() => resolve_subpath(&project, raw).join("working-memory"),
                _ => state.join("working-memory"),
            };
            let store = WorkingMemoryStore::open(&wm_dir)
                .map_err(|e| RuntimeError::unavailable(e.to_string()))?;
            store
                .write_note(scope_id, key, kind, status, text, &sources, ttl_seconds)
                .map_err(|e| RuntimeError::invalid(e.to_string()))
        }
        "omnigraph_status" => {
            let services = cache.for_state(&state, &project).await?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let mut query = Map::new();
            query.insert("project_id".into(), json!(project_id));
            if let Some(branch) = args.get("branch").and_then(Value::as_str) {
                query.insert("branch".into(), json!(branch));
            }
            let reply = services
                .request(
                    "omnigraph.status",
                    json!({ "params": {}, "query": Value::Object(query), "body": {} }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "omnigraph_atoms" => {
            let services = cache.for_state(&state, &project).await?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let mut query = Map::new();
            query.insert("project_id".into(), json!(project_id));
            if let Some(branch) = args.get("branch").and_then(Value::as_str) {
                query.insert("branch".into(), json!(branch));
            }
            if let Some(snapshot) = args.get("snapshot").and_then(Value::as_str) {
                query.insert("snapshot".into(), json!(snapshot));
            }
            if let Some(kind) = args.get("kind").and_then(Value::as_str) {
                query.insert("kind".into(), json!(kind));
            }
            if let Some(q) = args
                .get("q")
                .and_then(Value::as_str)
                .or_else(|| args.get("query").and_then(Value::as_str))
            {
                query.insert("q".into(), json!(q));
            }
            if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
                query.insert("session_id".into(), json!(sid));
            }
            if let Some(limit) = args.get("limit").and_then(Value::as_u64) {
                query.insert("limit".into(), json!(limit.to_string()));
            }
            if let Some(offset) = args.get("offset").and_then(Value::as_u64) {
                query.insert("offset".into(), json!(offset.to_string()));
            }
            if let Some(cursor) = args.get("cursor").and_then(Value::as_str) {
                query.insert("cursor".into(), json!(cursor));
            }
            let op = if args.get("search").and_then(Value::as_bool) == Some(true) {
                "omnigraph.search"
            } else {
                "omnigraph.graph"
            };
            let reply = services
                .request(
                    op,
                    json!({ "params": {}, "query": Value::Object(query), "body": {} }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "omnigraph_publish" => {
            let services = cache.for_state(&state, &project).await?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let mut body = Map::new();
            body.insert("project_id".into(), json!(project_id));
            for key in [
                "title",
                "kind",
                "summary",
                "body",
                "branch",
                "atom_id",
                "session_id",
                "tags",
                "files",
                "confidence",
                "pin_priority",
                "expected_commit",
            ] {
                if let Some(val) = args.get(key)
                    && !val.is_null()
                {
                    body.insert(key.into(), val.clone());
                }
            }
            let reply = services
                .request(
                    "omnigraph.publish",
                    json!({ "params": {}, "query": {}, "body": Value::Object(body) }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "omnigraph_branches_diff" => {
            let services = cache.for_state(&state, &project).await?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let source = args["source"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `source`"))?;
            let target = args
                .get("target")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("main");
            let reply = services
                .request(
                    "omnigraph.branch_diff",
                    json!({
                        "params": {},
                        "query": { "project_id": project_id, "source": source, "target": target },
                        "body": {}
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "omnigraph_merge_keep" => {
            let services = cache.for_state(&state, &project).await?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let atom_id = args["atom_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `atom_id`"))?;
            let from = args["from"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `from`"))?;
            let to = args
                .get("to")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("main");
            let mut body = json!({
                "action": "keep",
                "project_id": project_id,
                "atom_id": atom_id,
                "from": from,
                "to": to,
            });
            if let Some(v) = args.get("expected_updated_at").and_then(Value::as_str) {
                body["expected_updated_at"] = json!(v);
            }
            if let Some(v) = args.get("replaced_updated_at").and_then(Value::as_str) {
                body["replaced_updated_at"] = json!(v);
            }
            let reply = services
                .request(
                    "omnigraph.branch_act",
                    json!({ "params": {}, "query": {}, "body": body }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "system_graph_status" => {
            let services = cache.for_state(&state, &project).await?;
            let project_id = ensure_project(services.as_ref(), &project).await?;
            let mut query = Map::new();
            query.insert("project_id".into(), json!(project_id));
            if let Some(cfg) = args.get("config_path").and_then(Value::as_str)
                && !cfg.trim().is_empty()
            {
                query.insert("config_path".into(), json!(cfg));
            }
            if let Some(check) = args.get("check").and_then(Value::as_bool) {
                query.insert("check".into(), json!(if check { "1" } else { "0" }));
            }
            if let Some(svc) = args.get("service").and_then(Value::as_str)
                && !svc.trim().is_empty()
            {
                query.insert("service".into(), json!(svc));
            }
            let reply = services
                .request(
                    "system_graph.status",
                    json!({ "params": {}, "query": Value::Object(query), "body": {} }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        "orch_permission_mode" => {
            let services = cache.for_state(&state, &project).await?;
            let sid = args["session_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| RuntimeError::invalid("Missing required `session_id`"))?;
            if let Some(req_id) = args
                .get("request_id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                let decision = args["decision"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| RuntimeError::invalid("Missing required `decision` when `request_id` is set"))?;
                let mut body = json!({ "decision": decision });
                if let Some(answers) = args.get("answers")
                    && !answers.is_null()
                {
                    body["answers"] = answers.clone();
                }
                let reply = services
                    .request(
                        "sessions.permission",
                        json!({
                            "params": { "session_id": sid, "request_id": req_id },
                            "query": {},
                            "body": body
                        }),
                    )
                    .await
                    .map_err(|e| RuntimeError::invalid(e.message))?;
                return payload_json(reply);
            }
            if args.get("revoke_all").and_then(Value::as_bool) == Some(true)
                || args.get("revoke_rule_id").and_then(Value::as_str).is_some()
                || args.get("revoke_rule_key").and_then(Value::as_str).is_some()
            {
                let mut body = Map::new();
                if let Some(all) = args.get("revoke_all").and_then(Value::as_bool) {
                    body.insert("all".into(), json!(all));
                }
                if let Some(id) = args.get("revoke_rule_id").and_then(Value::as_str) {
                    body.insert("id".into(), json!(id));
                }
                if let Some(rk) = args.get("revoke_rule_key").and_then(Value::as_str) {
                    body.insert("rule_key".into(), json!(rk));
                }
                let reply = services
                    .request(
                        "sessions.permission_rules_delete",
                        json!({
                            "params": { "session_id": sid },
                            "query": {},
                            "body": Value::Object(body)
                        }),
                    )
                    .await
                    .map_err(|e| RuntimeError::invalid(e.message))?;
                return payload_json(reply);
            }
            let mut body = Map::new();
            if let Some(mode) = args.get("mode").and_then(Value::as_str) {
                body.insert("mode".into(), json!(mode));
            }
            let reply = services
                .request(
                    "sessions.permission_mode",
                    json!({
                        "params": { "session_id": sid },
                        "query": {},
                        "body": Value::Object(body)
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            payload_json(reply)
        }
        other => Err(RuntimeError::not_found(format!("Unknown MCP tool `{other}`"))),
    }
}

pub async fn serve_stdio() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let cache = RuntimeCache::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {err}") }
                });
                writeln!(writer, "{reply}")?;
                writer.flush()?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        let method = request["method"].as_str().unwrap_or("");
        if id.is_none() {
            // Notification such as `notifications/initialized`.
            continue;
        }
        let id = id.unwrap();
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "system-orchestration",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": "Pass `project` or `root` pointing to the target repository root on every tool call (especially under Codex where server cwd is the plugin cache directory)."
                }
            }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": tools_list() }),
            "tools/call" => {
                let params = &request["params"];
                let name = params["name"].as_str().unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match dispatch_tool(&cache, name, &args).await {
                    Ok(value) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string_pretty(&value)?
                            }],
                            "structuredContent": value,
                            "isError": false
                        }
                    }),
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": err.to_string()
                            }],
                            "isError": true
                        }
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") }
            }),
        };
        writeln!(writer, "{response}")?;
        writer.flush()?;
    }
    cache.close_all().await;
    Ok(())
}
