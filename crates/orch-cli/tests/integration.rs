use orch_harness::{
    desktop::{ServerConfig, ServerControl, ServicesFactory},
    http,
    native::NativeFactory,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_orch-engine")
}

fn init_git_repo(dir: &Path) {
    for args in [
        &["init"][..],
        &["config", "user.name", "Tester"][..],
        &["config", "user.email", "tester@example.com"][..],
    ] {
        let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{:?}", out);
    }
    std::fs::write(dir.join("README.md"), "# Fixture\n").unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "init"][..]] {
        let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{:?}", out);
    }
}

fn rpc(stdin: &mut impl Write, reader: &mut impl BufRead, req: Value) -> Value {
    writeln!(stdin, "{req}").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn mcp_tools_list_exposes_all_25_tools() {
    let list = orch_cli::mcp::tools_list();
    let tools = list["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 25);
    let names = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    for expected in [
        "system_init",
        "system_catalog",
        "system_locate",
        "system_impact",
        "system_plan",
        "system_execute",
        "system_resume",
        "system_run_status",
        "orch_session_start",
        "orch_session_send",
        "orch_session_status",
        "orch_session_cancel",
        "orch_context_draft",
        "orch_fleet_review",
        "orch_clash_debate",
        "orch_delivery_preview",
        "orch_working_memory_read",
        "orch_working_memory_write",
        "omnigraph_status",
        "omnigraph_atoms",
        "omnigraph_publish",
        "omnigraph_branches_diff",
        "omnigraph_merge_keep",
        "system_graph_status",
        "orch_permission_mode",
    ] {
        assert!(names.contains(expected), "missing MCP tool {expected}");
    }
}

#[test]
fn binary_cli_and_mcp_stdio_roundtrip() {
    let help = Command::new(bin()).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("orch-engine"));

    let ver = Command::new(bin()).arg("--version").output().unwrap();
    assert!(ver.status.success());
    assert!(String::from_utf8_lossy(&ver.stdout).contains("0.1.0"));

    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_git_repo(&repo);

    // Hook smoke check (fail-open when no .orch dispatch lock active).
    let mut hook_child = Command::new(bin())
        .args(["hook", "subagent-gate", "--host", "claude", "--project"])
        .arg(&repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hook_child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_name":"Bash","tool_input":{"command":"git status"}}"#)
        .unwrap();
    let hook_out = hook_child.wait_with_output().unwrap();
    assert_eq!(hook_out.status.code(), Some(0));

    // Attach dry-run smoke check.
    let attach = Command::new(bin())
        .args(["attach", "--dry-run", "--provider", "codex", "--project"])
        .arg(&repo)
        .arg("--state-dir")
        .arg(tmp.path().join("attach-state"))
        .output()
        .unwrap();
    assert!(attach.status.success());
    let attach_json: Value = serde_json::from_slice(&attach.stdout).unwrap();
    assert_eq!(attach_json["ok"], true);
    assert_eq!(attach_json["dry_run"], true);

    // Prepare service subdirectories inside repo for system orchestration MCP tests.
    let sys_dir = tmp.path().join("sysroot");
    let svc_a = repo.join("orders");
    let svc_b = repo.join("billing");
    std::fs::create_dir_all(&svc_a).unwrap();
    std::fs::create_dir_all(&svc_b).unwrap();
    std::fs::write(
        svc_a.join("ai-service.json"),
        serde_json::to_string_pretty(&json!({
            "schema_version": 1,
            "id": "orders",
            "description": "Orders service",
            "owner": "orders-team",
            "capabilities": [{
                "id": "order-intake",
                "description": "Accepts orders and emits order.placed",
                "status": "implemented",
                "sources": ["README.md"]
            }],
            "provides": [{
                "id": "order.placed",
                "kind": "event",
                "version": "1.0",
                "sources": ["README.md"]
            }],
            "consumes": [],
            "sources": [{ "path": "README.md", "kind": "spec" }]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(svc_a.join("README.md"), "orders service contract order.placed\n").unwrap();
    std::fs::write(
        svc_b.join("ai-service.json"),
        serde_json::to_string_pretty(&json!({
            "schema_version": 1,
            "id": "billing",
            "description": "Billing service",
            "owner": "billing-team",
            "capabilities": [{
                "id": "invoice-create",
                "description": "Creates invoices on order.placed",
                "status": "implemented",
                "sources": ["README.md"]
            }],
            "provides": [],
            "consumes": [{
                "service": "orders",
                "contract": "order.placed",
                "version": "1.0"
            }],
            "sources": [{ "path": "README.md", "kind": "spec" }]
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(svc_b.join("README.md"), "billing consumer of order.placed\n").unwrap();

    // Spawn `orch-engine mcp` and test real JSON-RPC 2.0 roundtrip.
    let mut mcp = Command::new(bin())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());

    let init = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {} }
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .unwrap();
    stdin.flush().unwrap();

    let list = rpc(
        &mut stdin,
        &mut stdout,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    );
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 25);
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    for expected in [
        "omnigraph_status",
        "omnigraph_atoms",
        "omnigraph_publish",
        "omnigraph_branches_diff",
        "omnigraph_merge_keep",
        "system_graph_status",
        "orch_permission_mode",
    ] {
        assert!(tool_names.contains(&expected), "missing MCP tool {expected}");
    }

    let s_init = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "system_init",
                "arguments": { "root": sys_dir, "name": "Commerce" }
            }
        }),
    );
    assert_eq!(s_init["result"]["isError"], false, "{s_init}");
    let mut sys_cfg: Value = serde_json::from_str(&std::fs::read_to_string(sys_dir.join("system.json")).unwrap()).unwrap();
    sys_cfg["services"] = json!([
        { "id": "orders", "root": svc_a, "manifest": "ai-service.json" },
        { "id": "billing", "root": svc_b, "manifest": "ai-service.json" }
    ]);
    std::fs::write(
        sys_dir.join("system.json"),
        serde_json::to_string_pretty(&sys_cfg).unwrap(),
    )
    .unwrap();

    let s_impact = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "system_impact",
                "arguments": {
                    "root": sys_dir,
                    "service": ["orders"],
                    "allow_root": [repo]
                }
            }
        }),
    );
    assert_eq!(s_impact["result"]["isError"], false, "{s_impact}");
    let impacted_ids = s_impact["result"]["structuredContent"]["impacted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["service"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(impacted_ids.contains(&"billing".to_string()), "{s_impact}");

    let s_plan = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "system_plan",
                "arguments": {
                    "root": sys_dir,
                    "task": "Update order.placed payload",
                    "change_id": "chg-001",
                    "service": ["orders"],
                    "allow_root": [repo],
                    "output": sys_dir.join("plan.json")
                }
            }
        }),
    );
    assert_eq!(s_plan["result"]["isError"], false, "{s_plan}");
    assert!(sys_dir.join("plan.json").is_file());

    let wm_write = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "orch_working_memory_write",
                "arguments": {
                    "project": repo,
                    "scope_id": "run-alpha",
                    "key": "order-schema",
                    "kind": "fact",
                    "status": "open",
                    "text": "orders emits order.placed consumed by billing"
                }
            }
        }),
    );
    assert_eq!(wm_write["result"]["isError"], false, "{wm_write}");

    let wm_read = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "orch_working_memory_read",
                "arguments": {
                    "project": repo,
                    "scope_id": "run-alpha"
                }
            }
        }),
    );
    assert_eq!(wm_read["result"]["isError"], false, "{wm_read}");
    assert!(
        wm_read["result"]["structuredContent"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["key"] == "order-schema")
    );

    let fleet = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "orch_fleet_review",
                "arguments": {
                    "project": repo,
                    "worker": "dry-run",
                    "thread": "mcp-fleet-001",
                    "lenses": ["code-reviewer", "security-reviewer"]
                }
            }
        }),
    );
    assert_eq!(fleet["result"]["isError"], false, "{fleet}");
    assert_eq!(
        fleet["result"]["structuredContent"]["result"]["status"],
        "awaiting_approval"
    );

    let fleet_resume = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "orch_fleet_review",
                "arguments": {
                    "project": repo,
                    "worker": "dry-run",
                    "thread": "mcp-fleet-001",
                    "lenses": ["code-reviewer", "security-reviewer"],
                    "resume": true,
                    "approve": true
                }
            }
        }),
    );
    assert_eq!(fleet_resume["result"]["isError"], false, "{fleet_resume}");
    assert_eq!(
        fleet_resume["result"]["structuredContent"]["result"]["status"],
        "completed"
    );

    let sg_status = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "system_graph_status",
                "arguments": { "project": sys_dir, "check": true }
            }
        }),
    );
    assert_eq!(sg_status["result"]["isError"], false, "{sg_status}");

    let omni_status = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "omnigraph_status",
                "arguments": { "project": repo }
            }
        }),
    );
    assert_eq!(omni_status["result"]["isError"], false, "{omni_status}");
    assert_eq!(omni_status["result"]["structuredContent"]["mode"], "off");

    let omni_atoms = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {
                "name": "omnigraph_atoms",
                "arguments": { "project": repo, "q": "loopback", "limit": 20 }
            }
        }),
    );
    assert_eq!(omni_atoms["result"]["isError"], false, "{omni_atoms}");
    assert_eq!(omni_atoms["result"]["structuredContent"]["connected"], false);
    assert_eq!(omni_atoms["result"]["structuredContent"]["limit"], 20);

    for (id, tool, args) in [
        (
            12,
            "omnigraph_publish",
            json!({
                "project": repo,
                "atom_id": "mcp-atom-1",
                "kind": "decision",
                "title": "Isolate HTTP bridge on loopback"
            }),
        ),
        (
            130,
            "omnigraph_atoms",
            json!({ "project": repo, "q": "loopback", "search": true }),
        ),
        (
            14,
            "omnigraph_branches_diff",
            json!({ "project": repo, "source": "feat/test", "target": "main" }),
        ),
    ] {
        let res = rpc(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": tool, "arguments": args }
            }),
        );
        assert_eq!(res["result"]["isError"], true, "{tool}: {res}");
        assert!(
            res["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .contains("mode=off"),
            "{tool}: {res}"
        );
    }

    let sess_start = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 15,
            "method": "tools/call",
            "params": {
                "name": "orch_session_start",
                "arguments": {
                    "project": repo,
                    "prompt": "Inspect config",
                    "workflow": "plan",
                    "mode": "plan",
                    "permissions": "manual"
                }
            }
        }),
    );
    assert_eq!(sess_start["result"]["isError"], false, "{sess_start}");
    let sid = sess_start["result"]["structuredContent"]["session"]["id"]
        .as_str()
        .expect("started session id")
        .to_owned();

    let perm_update = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 16,
            "method": "tools/call",
            "params": {
                "name": "orch_permission_mode",
                "arguments": {
                    "project": repo,
                    "session_id": sid,
                    "mode": "auto"
                }
            }
        }),
    );
    assert_eq!(perm_update["result"]["isError"], false, "{perm_update}");
    assert_eq!(
        perm_update["result"]["structuredContent"]["permissions"],
        "auto"
    );

    let keep_err = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 17,
            "method": "tools/call",
            "params": {
                "name": "omnigraph_merge_keep",
                "arguments": {
                    "project": repo,
                    "atom_id": "mcp-atom-1",
                    "from": "main",
                    "to": "main"
                }
            }
        }),
    );
    assert_eq!(keep_err["result"]["isError"], true, "{keep_err}");

    drop(stdin);
    let status = mcp.wait().unwrap();
    assert!(status.success());

    // CLI smoke for `omnigraph`, `session permission`, and `system graph-status`
    let cli_omni = Command::new(bin())
        .args(["omnigraph", "status", "--project"])
        .arg(&repo)
        .output()
        .unwrap();
    assert!(cli_omni.status.success(), "{}", String::from_utf8_lossy(&cli_omni.stderr));

    let cli_perm = Command::new(bin())
        .args(["session", "permission", "--project"])
        .arg(&repo)
        .args(["--id", &sid, "--mode", "bypass"])
        .output()
        .unwrap();
    assert!(cli_perm.status.success(), "{}", String::from_utf8_lossy(&cli_perm.stderr));
    let cli_perm_json: Value = serde_json::from_slice(&cli_perm.stdout).unwrap();
    assert_eq!(cli_perm_json["permissions"], "bypass");

    let cli_sg = Command::new(bin())
        .args(["system", "graph-status", "--project"])
        .arg(&sys_dir)
        .output()
        .unwrap();
    assert!(cli_sg.status.success(), "{}", String::from_utf8_lossy(&cli_sg.stderr));
}

#[test]
fn binary_mcp_refuses_plugin_cwd_under_ancestor_git() {
    let tmp = tempdir().unwrap();
    // Simulate ancestor .git (e.g. ~/.git or cloned marketplace repo).
    let ancestor = tmp.path().join("marketplace");
    std::fs::create_dir(&ancestor).unwrap();
    init_git_repo(&ancestor);

    let plugin = ancestor.join("plugins").join("system-orchestration");
    std::fs::create_dir_all(plugin.join(".codex-plugin")).unwrap();
    std::fs::write(plugin.join(".codex-plugin/plugin.json"), "{}").unwrap();
    std::fs::create_dir_all(plugin.join("bin")).unwrap();
    std::fs::write(plugin.join("bin/orch-engine"), "").unwrap();

    let mut mcp = Command::new(bin())
        .arg("mcp")
        .current_dir(&plugin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());

    let _ = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {} }
        }),
    );
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .unwrap();
    stdin.flush().unwrap();

    for (id, tool, args) in [
        (2, "orch_working_memory_read", json!({ "scope_id": "test" })),
        (3, "system_init", json!({ "name": "Unsafe" })),
        (4, "orch_session_status", json!({})),
        (5, "orch_working_memory_read", json!({ "project": ".", "scope_id": "test" })),
        (6, "omnigraph_status", json!({})),
        (7, "omnigraph_publish", json!({ "title": "Unsafe" })),
        (8, "system_graph_status", json!({ "project": "." })),
        (9, "orch_permission_mode", json!({ "session_id": "s-1", "mode": "manual" })),
    ] {
        let resp = rpc(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": tool, "arguments": args }
            }),
        );
        assert_eq!(resp["result"]["isError"], true, "{tool}: {resp}");
    }
    drop(stdin);
    assert!(mcp.wait().unwrap().success());
    assert!(!ancestor.join(".orch").exists());
    assert!(!plugin.join(".orch").exists());
}

#[tokio::test]
async fn embedded_wasm_http_workbench_serves_trimmed_assets_and_working_memory_api() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_git_repo(&repo);
    let state = tmp.path().join("state");

    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let config = ServerConfig {
        source: repo.clone(),
        state,
        port,
        timeout: 600,
        session_concurrency: 1,
        projects: vec![repo.clone()],
        providers: BTreeMap::new(),
        path_env: std::env::var_os("PATH"),
    };
    let control = Arc::new(ServerControl {
        config: config.clone(),
        signal: shutdown.clone(),
    });
    let services = NativeFactory.create(&config, control).await.unwrap();
    let app = http::App::from_assets(
        services.clone(),
        port,
        repo,
        shutdown.clone(),
        orch_cli::assets::embedded_assets(),
    )
    .unwrap();
    let token = app.token.clone();
    let server = tokio::spawn(http::serve_listener(listener, app));

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    for path in [
        "/",
        "/app.css",
        "/boot.js",
        "/icons/logo.svg",
        "/assets/orch_ui.js",
        "/assets/orch_ui_bg.wasm",
    ] {
        let resp = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(resp.status(), 200, "GET {path}");
    }

    let boot: Value = serde_json::from_str(
        &client
            .get(format!("{base}/api/bootstrap"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(boot["runtime"]["working_memory"], true);
    assert!(boot.get("accelerators").is_none());

    // Verify GET /api/app/update returns 200 unsupported state (zero 404 console errors).
    let update_resp = client.get(format!("{base}/api/app/update?check=1")).send().await.unwrap();
    assert_eq!(update_resp.status(), 200);
    let update_json: Value = serde_json::from_str(&update_resp.text().await.unwrap()).unwrap();
    assert_eq!(update_json["supported"], false);
    assert_eq!(update_json["available"], false);
    let update_bad = client.get(format!("{base}/api/app/update?check=no")).send().await.unwrap();
    assert_eq!(update_bad.status(), 400);

    // Verify optional catalog autoload returns 200 empty-state when system.json is absent,
    // while explicit load without optional still returns 404.
    let project_id = boot["projects"][0]["id"].as_str().unwrap();
    let opt_catalog = client
        .post(format!("{base}/api/systems/catalog"))
        .header("x-harness-token", &token)
        .header("content-type", "application/json")
        .body(json!({"project_id": project_id, "config_path": "system.json", "optional": true}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(opt_catalog.status(), 200);
    let opt_json: Value = serde_json::from_str(&opt_catalog.text().await.unwrap()).unwrap();
    assert_eq!(opt_json["missing"], true);
    assert!(opt_json["catalog"].is_null());

    let strict_catalog = client
        .post(format!("{base}/api/systems/catalog"))
        .header("x-harness-token", &token)
        .header("content-type", "application/json")
        .body(json!({"project_id": project_id, "config_path": "system.json"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(strict_catalog.status(), 404);

    // Verify HTML fixes (DOM nesting inside <main>, closed single #panel-project, theme IDs, collapsed #model-hint) and CSS select fixes.
    let html = client.get(format!("{base}/")).send().await.unwrap().text().await.unwrap();
    assert!(html.contains(r#"id="theme-dark""#));
    assert!(html.contains(r#"id="model-hint" data-visible="false""#));
    assert_eq!(html.matches(r#"id="panel-project""#).count(), 1, "duplicate #panel-project in HTML");
    let main_start = html.find(r#"<main class="main" id="main""#).unwrap();
    let main_end = html.rfind("</main>").unwrap();
    let main_slice = &html[main_start..main_end];
    for inside_main in [
        r#"id="composer-area""#,
        r#"id="workspace-resizer""#,
        r#"id="sessions-right-pane""#,
        r#"id="results-view""#,
        r#"id="systems-view""#,
        r#"id="system-changes-view""#,
        r#"id="omnigraph-view""#,
    ] {
        assert!(main_slice.contains(inside_main), "{inside_main} escaped <main>");
    }
    let wasm_bytes = client
        .get(format!("{base}/assets/orch_ui_bg.wasm"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(&wasm_bytes[..4], b"\0asm");

    let graph_http = client
        .get(format!("{base}/api/projects/{project_id}/graph?check=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(graph_http.status(), 200, "graph status should be registered (not 503)");
    let perms_wait = client
        .get(format!("{base}/api/permissions/waiting"))
        .send()
        .await
        .unwrap();
    assert_eq!(perms_wait.status(), 200);
    let omni_http = client
        .get(format!("{base}/api/omnigraph/status?project_id={project_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(omni_http.status(), 200);
    let sg_missing = client
        .get(format!("{base}/api/systems/graph/status?project_id={project_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(sg_missing.status(), 404);
    let sse = client
        .get(format!("{base}/api/events/stream?project_id={project_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(sse.status(), 200);
    assert!(
        sse.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream")),
        "expected SSE content-type"
    );
    drop(sse);
    let css = client.get(format!("{base}/app.css")).send().await.unwrap().text().await.unwrap();
    assert!(css.contains("-webkit-appearance:none"));
    assert!(css.contains(".field:has(#workflow)"));
    assert!(css.contains("text-overflow:ellipsis"));
    assert!(css.contains("white-space:nowrap"));

    let wrote = client
        .post(format!("{base}/api/working-memory"))
        .header("x-harness-token", &token)
        .header("content-type", "application/json")
        .body(
            json!({
                "scope_id": "sess-1",
                "key": "auth-check",
                "kind": "decision",
                "status": "open",
                "text": "Keep HTTP server bound strictly to 127.0.0.1."
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(wrote.status(), 200);

    let read_back: Value = serde_json::from_str(
        &client
            .get(format!("{base}/api/working-memory?scope_id=sess-1"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(read_back["entries"].as_array().unwrap().len(), 1);
    assert_eq!(read_back["entries"][0]["key"], "auth-check");

    shutdown.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    services.close().await.unwrap();
}

#[test]
fn antigravity_hook_cli_and_gemini_plugin_cwd_guard() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".antigravity/agents")).unwrap();
    init_git_repo(&repo);
    std::fs::write(
        repo.join(".antigravity/agents/php-writer.md"),
        "---\nname: php-writer\nwrites: true\n---\n\nWrite agent\n",
    )
    .unwrap();

    // 1. CLI hook `--host antigravity` / `--host agy` + auto-detect camelCase stdin JSON
    let run_hook = |args: &[&str], body: &Value| {
        let mut child = Command::new(bin())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(serde_json::to_string(body).unwrap().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };

    let gate_payload = json!({
        "hookEventName": "PreToolUse",
        "conversationId": "conv-agy-1",
        "workspacePaths": [&repo],
        "toolCall": {
            "name": "invoke_subagent",
            "args": {
                "TypeName": "php-writer",
                "Prompt": "Modify service",
                "Workspace": "inherit"
            }
        }
    });
    let first = run_hook(&["hook", "subagent-gate", "--host", "antigravity"], &gate_payload);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(
        serde_json::from_slice::<Value>(&first.stdout).unwrap(),
        json!({ "decision": "allow" })
    );

    let deny = run_hook(&["hook", "subagent-gate", "--host", "agy"], &gate_payload);
    assert_eq!(deny.status.code(), Some(0));
    let deny_json: Value = serde_json::from_slice(&deny.stdout).unwrap();
    assert_eq!(deny_json["decision"], "deny");
    assert!(deny_json["reason"].as_str().unwrap().contains("php-writer"));

    let disp_payload = json!({
        "hookEventName": "PostToolUse",
        "conversationId": "conv-agy-1",
        "workspacePaths": [&repo],
        "toolCall": {
            "name": "invoke_subagent",
            "args": {
                "Role": "php-writer",
                "Prompt": "Modified service",
                "Workspace": "inherit"
            }
        }
    });
    let disp = run_hook(&["hook", "subagent-dispatch"], &disp_payload);
    assert_eq!(disp.status.code(), Some(0));
    assert_eq!(serde_json::from_slice::<Value>(&disp.stdout).unwrap(), json!({}));

    let again = run_hook(&["hook", "subagent-gate"], &gate_payload);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(
        serde_json::from_slice::<Value>(&again.stdout).unwrap(),
        json!({ "decision": "allow" })
    );
    let _ = run_hook(&["hook", "subagent-dispatch", "--host", "antigravity"], &disp_payload);

    // 2. `.gemini/config/plugins/<name>/` with plugin.json + mcp_config.json triggers plugin-cwd guard
    let agy_plugin = tmp
        .path()
        .join(".gemini")
        .join("config")
        .join("plugins")
        .join("system-orchestration");
    std::fs::create_dir_all(&agy_plugin).unwrap();
    std::fs::write(agy_plugin.join("plugin.json"), "{}").unwrap();
    std::fs::write(agy_plugin.join("mcp_config.json"), "{}").unwrap();

    let mut mcp = Command::new(bin())
        .arg("mcp")
        .current_dir(&agy_plugin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap());

    let _ = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {} }
        }),
    );
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .unwrap();
    stdin.flush().unwrap();

    for (id, tool, args) in [
        (2, "system_graph_status", json!({})),
        (3, "omnigraph_status", json!({ "project": "." })),
        (4, "orch_session_start", json!({ "project": "./.", "prompt": "p", "provider": "codex", "mode": "plan" })),
    ] {
        let resp = rpc(
            &mut stdin,
            &mut stdout,
            json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": tool, "arguments": args } }),
        );
        assert_eq!(resp["result"]["isError"], true, "{tool}: {resp}");
    }

    // Explicit external repo still succeeds from Antigravity plugin cwd!
    let ok_call = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "omnigraph_status", "arguments": { "project": repo } }
        }),
    );
    assert_eq!(ok_call["result"]["isError"], false, "{ok_call}");

    drop(stdin);
    assert!(mcp.wait().unwrap().success());
    assert!(!agy_plugin.join(".orch").exists());
}
