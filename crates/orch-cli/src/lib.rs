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
        "  orch-engine system <init|catalog|validate|map|locate|plan|verify|execute|resume|run-status|diagram> [OPTIONS]\n",
        "  orch-engine session <start|send|status|list|cancel|context-draft> [OPTIONS]\n",
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
            "Usage: orch-engine session <start|send|status|list|cancel|context-draft> [OPTIONS]"
        );
        return Ok(0);
    };
    if matches!(sub, "-h" | "--help") {
        println!(
            "Usage: orch-engine session <start|send|status|list|cancel|context-draft> [--project PATH] [--state-dir PATH] ..."
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
            services
                .request(
                    "sessions.send",
                    json!({
                        "params": { "session_id": sid },
                        "query": {},
                        "body": { "prompt": prompt }
                    }),
                )
                .await
                .map_err(|e| RuntimeError::invalid(e.message))
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
        "system" => orch_harness::run("system", rest).await,
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
