pub mod assets;
pub mod mcp;

use orch_core::{Result, RuntimeError};
use orch_harness::{
    attach::{build_configured_overlay, run_attached_project, OverlayConfig},
    desktop::{default_state, ServerConfig, ServerControl, ServicesFactory},
    dispatch_hooks::execute_hook,
    native::NativeFactory,
    services::{Payload, Services},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

fn usage() -> &'static str {
    concat!(
        "orch-engine ",
        env!("CARGO_PKG_VERSION"),
        " — Standalone multi-service orchestration, session scheduler, working memory, & workbench\n\n",
        "USAGE:\n",
        "  orch-engine system <init|catalog|validate|map|locate|plan|verify|execute|resume|run-status|diagram|graph-status> [OPTIONS]\n",
        "  orch-engine session <start|send|status|list|cancel|context-draft|permission> [OPTIONS]\n",
        "  orch-engine omnigraph <status|atoms|publish|branch-diff|merge-keep> [OPTIONS]\n",
        "  orch-engine fleet <run|resume> [OPTIONS]\n",
        "  orch-engine serve [serve|start] [OPTIONS]\n",
        "  orch-engine attach [--project PATH] [--provider claude|codex|cursor] [--state-dir PATH] [--overlay-dir PATH] [-- ARGS...]\n",
        "  orch-engine hook <subagent-gate|subagent-dispatch> [--project PATH] [--host claude|codex|cursor]\n",
        "  orch-engine mcp\n",
        "  orch-engine --help | --version\n"
    )
}

fn take_opt(args: &[String], name: &str) -> Result<Option<String>> {
    let flag = format!("--{name}");
    let prefix = format!("--{name}=");
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            let val = args
                .get(i + 1)
                .ok_or_else(|| RuntimeError::invalid(format!("Missing value for {flag}")))?;
            return Ok(Some(val.clone()));
        }
        if let Some(val) = args[i].strip_prefix(&prefix) {
            return Ok(Some(val.to_string()));
        }
        i += 1;
    }
    Ok(None)
}

fn has_flag(args: &[String], name: &str) -> bool {
    let flag = format!("--{name}");
    args.iter().any(|a| a == &flag)
}

fn resolve_state(raw: Option<String>, project: Option<&Path>) -> Result<PathBuf> {
    if let Some(s) = raw {
        let p = PathBuf::from(s);
        return Ok(if p.is_absolute() {
            p
        } else if let Some(proj) = project {
            proj.join(p)
        } else {
            std::env::current_dir()?.join(p)
        });
    }
    if let Some(proj) = project {
        return Ok(proj.join(".orch").join("state"));
    }
    default_state().map_err(|e| RuntimeError::unavailable(e.message))
}

async fn open_services(state: &Path, project: &Path) -> Result<Arc<dyn Services>> {
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
    NativeFactory
        .create(&config, control)
        .await
        .map_err(|e| RuntimeError::unavailable(e.message))
}

fn print_payload(payload: Payload) -> Result<i32> {
    match payload {
        Payload::Json(v) => {
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Payload::Bytes { bytes, .. } => {
            println!("{}", String::from_utf8_lossy(&bytes));
        }
    }
    Ok(0)
}

async fn run_session_cli(args: &[String]) -> Result<i32> {
    let Some(sub) = args.first().map(String::as_str) else {
        println!(
            "Usage: orch-engine session <start|send|status|list|cancel|context-draft|permission> [OPTIONS]"
        );
        return Ok(0);
    };
    if matches!(sub, "-h" | "--help") {
        println!(
            "Usage: orch-engine session <start|send|status|list|cancel|context-draft|permission> [--project PATH] [--state-dir PATH] ..."
        );
        return Ok(0);
    }
    let rest = &args[1..];
    let project_raw = take_opt(rest, "project")?;
    let project = match project_raw {
        Some(raw) => orch_core::fs::canonicalize(Path::new(&raw))?,
        None => orch_core::fs::canonicalize(&std::env::current_dir()?)?,
    };
    let state = resolve_state(take_opt(rest, "state-dir")?, Some(&project))?;
    let services = open_services(&state, &project).await?;
    let result = match sub {
        "list" => services
            .request("sessions.list", json!({ "params": {}, "query": {}, "body": {} }))
            .await
            .map_err(|e| RuntimeError::invalid(e.message)),
        "status" => {
            let sid = take_opt(rest, "id")?
                .ok_or_else(|| RuntimeError::invalid("Missing --id <SESSION_ID>"))?;
            let after = take_opt(rest, "after")?;
            let query = match after {
                Some(a) => json!({ "after": a }),
                None => json!({}),
            };
            services
                .request(
                    "sessions.get",
                    json!({ "params": { "session_id": sid }, "query": query, "body": {} }),
                )
                .await
                .map_err(|e| RuntimeError::not_found(e.message))
        }
        "cancel" => {
            let sid = take_opt(rest, "id")?
                .ok_or_else(|| RuntimeError::invalid("Missing --id <SESSION_ID>"))?;
            services
                .request(
                    "sessions.cancel",
                    json!({ "params": { "session_id": sid }, "query": {}, "body": {} }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
        }
        "context-draft" => {
            let sid = take_opt(rest, "id")?
                .ok_or_else(|| RuntimeError::invalid("Missing --id <SESSION_ID>"))?;
            let proj_list = services
                .request("projects.list", json!({ "params": {}, "query": {}, "body": {} }))
                .await
                .map_err(|e| RuntimeError::unavailable(e.message))?;
            let pid = match proj_list {
                Payload::Json(v) => v["projects"]
                    .as_array()
                    .and_then(|arr| arr.iter().find(|p| p["path"] == project.to_string_lossy().as_ref()))
                    .and_then(|p| p["id"].as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
            services
                .request(
                    "sessions.context_draft",
                    json!({
                        "params": { "session_id": sid },
                        "query": { "project_id": pid },
                        "body": {}
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
        }
        "start" => {
            let prompt = take_opt(rest, "prompt")?
                .ok_or_else(|| RuntimeError::invalid("Missing --prompt <TEXT>"))?;
            let provider = take_opt(rest, "provider")?.unwrap_or_else(|| "claude".into());
            let workflow = take_opt(rest, "workflow")?.unwrap_or_else(|| "native".into());
            let mode = take_opt(rest, "mode")?.unwrap_or_else(|| "plan".into());
            let proj_added = services
                .request(
                    "projects.add",
                    json!({ "params": {}, "query": {}, "body": { "path": project.to_string_lossy() } }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))?;
            let pid = match proj_added {
                Payload::Json(v) => v["project"]["id"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
            let mut body = json!({
                "project_id": pid,
                "provider": provider,
                "workflow": workflow,
                "mode": mode,
                "workspace": "project",
                "prompt": prompt
            });
            if let Some(perms) = take_opt(rest, "permissions")? {
                body["permissions"] = json!(perms);
            }
            if workflow == "fleet-review" && has_flag(rest, "dry-run") {
                body["fleet"] = json!({ "dry_run": true });
            }
            services
                .request(
                    "sessions.create",
                    json!({ "params": {}, "query": {}, "body": body }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
        }
        "send" => {
            let sid = take_opt(rest, "id")?
                .ok_or_else(|| RuntimeError::invalid("Missing --id <SESSION_ID>"))?;
            let prompt = take_opt(rest, "prompt")?
                .ok_or_else(|| RuntimeError::invalid("Missing --prompt <TEXT>"))?;
            let mut body = json!({ "prompt": prompt });
            if let Some(perms) = take_opt(rest, "permissions")? {
                body["permissions"] = json!(perms);
            }
            services
                .request(
                    "sessions.send",
                    json!({
                        "params": { "session_id": sid },
                        "query": {},
                        "body": body
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
        }
        "permission" => {
            let sid = take_opt(rest, "id")?
                .ok_or_else(|| RuntimeError::invalid("Missing --id <SESSION_ID>"))?;
            if let Some(req_id) = take_opt(rest, "request-id")? {
                let decision = take_opt(rest, "decision")?
                    .ok_or_else(|| RuntimeError::invalid("Missing --decision <allow|allow_session|deny>"))?;
                services
                    .request(
                        "sessions.permission",
                        json!({
                            "params": { "session_id": sid, "request_id": req_id },
                            "query": {},
                            "body": { "decision": decision }
                        }),
                    )
                    .await
                    .map_err(|e| RuntimeError::invalid(e.message))
            } else if has_flag(rest, "revoke-all")
                || take_opt(rest, "revoke-id")?.is_some()
                || take_opt(rest, "revoke-key")?.is_some()
            {
                let mut body = serde_json::Map::new();
                if has_flag(rest, "revoke-all") {
                    body.insert("all".into(), json!(true));
                }
                if let Some(id) = take_opt(rest, "revoke-id")? {
                    body.insert("id".into(), json!(id));
                }
                if let Some(rk) = take_opt(rest, "revoke-key")? {
                    body.insert("rule_key".into(), json!(rk));
                }
                services
                    .request(
                        "sessions.permission_rules_delete",
                        json!({
                            "params": { "session_id": sid },
                            "query": {},
                            "body": serde_json::Value::Object(body)
                        }),
                    )
                    .await
                    .map_err(|e| RuntimeError::invalid(e.message))
            } else {
                let mut body = serde_json::Map::new();
                if let Some(mode) = take_opt(rest, "mode")? {
                    body.insert("mode".into(), json!(mode));
                }
                services
                    .request(
                        "sessions.permission_mode",
                        json!({
                            "params": { "session_id": sid },
                            "query": {},
                            "body": serde_json::Value::Object(body)
                        }),
                    )
                    .await
                    .map_err(|e| RuntimeError::invalid(e.message))
            }
        }
        other => Err(RuntimeError::invalid(format!(
            "Unknown session subcommand `{other}`"
        ))),
    };
    let code = match result {
        Ok(payload) => print_payload(payload),
        Err(err) => {
            let _ = services.close().await;
            return Err(err);
        }
    };
    let _ = services.close().await;
    code
}

fn run_attach_cli(args: &[String]) -> Result<i32> {
    if args.iter().any(|a| matches!(a.as_str(), "-h" | "--help")) {
        println!("Usage: orch-engine attach [--project PATH] [--provider claude|codex|cursor] [--state-dir PATH] [--overlay-dir PATH] [--dry-run] [-- ARGS...]");
        return Ok(0);
    }
    let sep = args.iter().position(|a| a == "--");
    let (opts, passthrough): (&[String], &[String]) = match sep {
        Some(idx) => (&args[..idx], &args[idx + 1..]),
        None => (args, &[]),
    };
    let project = match take_opt(opts, "project")? {
        Some(raw) => orch_core::fs::canonicalize(Path::new(&raw))?,
        None => orch_core::fs::canonicalize(&std::env::current_dir()?)?,
    };
    let provider = take_opt(opts, "provider")?.unwrap_or_else(|| "claude".into());
    let state_base = take_opt(opts, "state-dir")?.map(PathBuf::from);
    let overlay_dir = take_opt(opts, "overlay-dir")?.map(PathBuf::from);
    if has_flag(opts, "dry-run") {
        let state = orch_harness::attach::state_directory(&project, state_base.as_deref())
            .map_err(|e| RuntimeError::invalid(e.message))?;
        let config = OverlayConfig {
            overlay_dir,
            system_prompt: None,
            hooks: None,
        };
        let overlay = build_configured_overlay(&project, &state, &provider, &config)
            .map_err(|e| RuntimeError::invalid(e.message))?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "dry_run": true,
                "project": project,
                "provider": provider,
                "state": state,
                "overlay": overlay
            }))?
        );
        return Ok(0);
    }
    run_attached_project(
        &project,
        &provider,
        overlay_dir.as_deref(),
        state_base.as_deref(),
        passthrough,
    )
    .map_err(|e| RuntimeError::unavailable(e.message))
}

fn run_hook_cli(args: &[String]) -> Result<i32> {
    let Some(op) = args.first().map(String::as_str) else {
        return Err(RuntimeError::invalid(
            "Missing hook name (`subagent-gate` or `subagent-dispatch`)",
        ));
    };
    if matches!(op, "-h" | "--help") {
        println!("Usage: orch-engine hook <subagent-gate|subagent-dispatch> [--project PATH] [--host claude|codex|cursor]");
        return Ok(0);
    }
    let rest = &args[1..];
    let project = take_opt(rest, "project")?.map(PathBuf::from);
    let host = take_opt(rest, "host")?;
    let mut stdin_buf = Vec::new();
    std::io::stdin().read_to_end(&mut stdin_buf)?;
    let outcome = execute_hook(op, project.as_deref(), host.as_deref(), &stdin_buf)?;
    if !outcome.stdout.is_empty() {
        use std::io::Write;
        std::io::stdout().write_all(&outcome.stdout)?;
    }
    if !outcome.stderr.is_empty() {
        use std::io::Write;
        std::io::stderr().write_all(&outcome.stderr)?;
    }
    Ok(outcome.code)
}

async fn ensure_registered_project(services: &Arc<dyn Services>, project: &Path) -> Result<String> {
    let added = services
        .request(
            "projects.add",
            json!({ "params": {}, "query": {}, "body": { "path": project.to_string_lossy() } }),
        )
        .await
        .map_err(|e| RuntimeError::invalid(e.message))?;
    match added {
        Payload::Json(v) => v["project"]["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| RuntimeError::unavailable("Registered project returned empty id")),
        Payload::Bytes { .. } => Err(RuntimeError::unavailable("Unexpected binary payload for projects.add")),
    }
}

async fn run_system_graph_status_cli(args: &[String]) -> Result<i32> {
    if args.iter().any(|a| matches!(a.as_str(), "-h" | "--help")) {
        println!("Usage: orch-engine system graph-status [--project PATH] [--state-dir PATH] [--config PATH] [--service ID] [--check]");
        return Ok(0);
    }
    let project = match take_opt(args, "project")? {
        Some(raw) => orch_core::fs::canonicalize(Path::new(&raw))?,
        None => orch_core::fs::canonicalize(&std::env::current_dir()?)?,
    };
    let state = resolve_state(take_opt(args, "state-dir")?, Some(&project))?;
    let services = open_services(&state, &project).await?;
    let pid = ensure_registered_project(&services, &project).await?;
    let mut query = serde_json::Map::new();
    query.insert("project_id".into(), json!(pid));
    if let Some(cfg) = take_opt(args, "config")?.or(take_opt(args, "system")?) {
        query.insert("config_path".into(), json!(cfg));
    }
    if has_flag(args, "check") {
        query.insert("check".into(), json!("1"));
    }
    if let Some(svc) = take_opt(args, "service")? {
        query.insert("service".into(), json!(svc));
    }
    let res = services
        .request(
            "system_graph.status",
            json!({ "params": {}, "query": serde_json::Value::Object(query), "body": {} }),
        )
        .await
        .map_err(|e| RuntimeError::invalid(e.message));
    let code = match res {
        Ok(p) => print_payload(p),
        Err(e) => {
            let _ = services.close().await;
            return Err(e);
        }
    };
    let _ = services.close().await;
    code
}

async fn run_omnigraph_cli(args: &[String]) -> Result<i32> {
    let Some(sub) = args.first().map(String::as_str) else {
        println!(
            "Usage: orch-engine omnigraph <status|atoms|publish|branch-diff|merge-keep> [--project PATH] [--state-dir PATH] ..."
        );
        return Ok(0);
    };
    if matches!(sub, "-h" | "--help") {
        println!(
            "Usage: orch-engine omnigraph <status|atoms|publish|branch-diff|merge-keep> [--project PATH] [--state-dir PATH] ..."
        );
        return Ok(0);
    }
    let rest = &args[1..];
    let project = match take_opt(rest, "project")? {
        Some(raw) => orch_core::fs::canonicalize(Path::new(&raw))?,
        None => orch_core::fs::canonicalize(&std::env::current_dir()?)?,
    };
    let state = resolve_state(take_opt(rest, "state-dir")?, Some(&project))?;
    let services = open_services(&state, &project).await?;
    let pid = ensure_registered_project(&services, &project).await?;
    let res = match sub {
        "status" => services
            .request(
                "omnigraph.status",
                json!({ "params": {}, "query": { "project_id": pid }, "body": {} }),
            )
            .await
            .map_err(|e| RuntimeError::invalid(e.message)),
        "atoms" => {
            let mut query = serde_json::Map::new();
            query.insert("project_id".into(), json!(pid));
            for (flag, key) in [
                ("branch", "branch"),
                ("snapshot", "snapshot"),
                ("kind", "kind"),
                ("q", "q"),
                ("query", "q"),
                ("session-id", "session_id"),
                ("limit", "limit"),
                ("offset", "offset"),
                ("cursor", "cursor"),
            ] {
                if let Some(val) = take_opt(rest, flag)? {
                    query.insert(key.into(), json!(val));
                }
            }
            let op = if has_flag(rest, "search") {
                "omnigraph.search"
            } else {
                "omnigraph.graph"
            };
            services
                .request(
                    op,
                    json!({ "params": {}, "query": serde_json::Value::Object(query), "body": {} }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
        }
        "publish" => {
            let title = take_opt(rest, "title")?
                .ok_or_else(|| RuntimeError::invalid("Missing --title <TEXT>"))?;
            let mut body = json!({
                "project_id": pid,
                "title": title,
            });
            for (flag, key) in [
                ("kind", "kind"),
                ("summary", "summary"),
                ("body", "body"),
                ("branch", "branch"),
                ("atom-id", "atom_id"),
                ("session-id", "session_id"),
            ] {
                if let Some(val) = take_opt(rest, flag)? {
                    body[key] = json!(val);
                }
            }
            if let Some(files_raw) = take_opt(rest, "files")? {
                let list: Vec<String> = files_raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                body["files"] = json!(list);
            }
            if let Some(tags_raw) = take_opt(rest, "tags")? {
                let list: Vec<String> = tags_raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                body["tags"] = json!(list);
            }
            services
                .request(
                    "omnigraph.publish",
                    json!({ "params": {}, "query": {}, "body": body }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
        }
        "branch-diff" => {
            let source = take_opt(rest, "source")?
                .ok_or_else(|| RuntimeError::invalid("Missing --source <BRANCH>"))?;
            let target = take_opt(rest, "target")?.unwrap_or_else(|| "main".into());
            services
                .request(
                    "omnigraph.branch_diff",
                    json!({
                        "params": {},
                        "query": { "project_id": pid, "source": source, "target": target },
                        "body": {}
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
        }
        "merge-keep" => {
            let atom_id = take_opt(rest, "atom-id")?
                .ok_or_else(|| RuntimeError::invalid("Missing --atom-id <ID>"))?;
            let from = take_opt(rest, "from")?
                .ok_or_else(|| RuntimeError::invalid("Missing --from <BRANCH>"))?;
            let to = take_opt(rest, "to")?.unwrap_or_else(|| "main".into());
            services
                .request(
                    "omnigraph.branch_act",
                    json!({
                        "params": {},
                        "query": {},
                        "body": {
                            "action": "keep",
                            "project_id": pid,
                            "atom_id": atom_id,
                            "from": from,
                            "to": to
                        }
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
        }
        other => Err(RuntimeError::invalid(format!(
            "Unknown omnigraph subcommand `{other}`"
        ))),
    };
    let code = match res {
        Ok(p) => print_payload(p),
        Err(e) => {
            let _ = services.close().await;
            return Err(e);
        }
    };
    let _ = services.close().await;
    code
}

pub async fn dispatch(args: &[String]) -> Result<i32> {
    let Some(first) = args.first().map(String::as_str) else {
        print!("{}", usage());
        return Ok(0);
    };
    if matches!(first, "-h" | "--help" | "help") {
        print!("{}", usage());
        return Ok(0);
    }
    if matches!(first, "-V" | "--version" | "version") {
        println!("orch-engine {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let rest = &args[1..];
    match first {
        "internal-guard" | "internal-sandbox-child" | "internal-discovery-probe" => {
            orch_harness::run(first, rest).await
        }
        "system" => {
            if rest.first().map(String::as_str) == Some("graph-status") {
                run_system_graph_status_cli(&rest[1..]).await
            } else {
                orch_harness::run("system", rest).await
            }
        }
        "omnigraph" => run_omnigraph_cli(rest).await,
        "fleet" | "harness" => orch_harness::run("fleet", rest).await,
        "serve" => orch_harness::facade::run_with_assets("serve", rest, assets::embedded_assets()).await,
        "session" => run_session_cli(rest).await,
        "attach" => run_attach_cli(rest),
        "hook" => run_hook_cli(rest),
        "mcp" => {
            mcp::serve_stdio().await?;
            Ok(0)
        }
        other => Err(RuntimeError::invalid(format!(
            "Unknown subcommand `{other}`. Run `orch-engine --help`."
        ))),
    }
}
