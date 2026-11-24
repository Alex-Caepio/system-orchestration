//! Standalone production composition for orchestration domains without monorepo or PHP deps.
use crate::{
    Result,
    bindings::{ResultsBridge, SessionLinks, SystemBridge, SystemWorkflow, SystemsDomain},
    blackboard::LocalBlackboards,
    desktop::{FactoryFuture, ServerConfig, ServerControl, ServicesFactory},
    provider::ProviderDomain,
    services::{Domain, NativeServices, Services},
    sessions::{SessionLimits, Sessions, SpecialWorkflow},
    store::SessionStore,
    working_memory::WorkingMemoryDomain,
};
use serde_json::{Value, json};
#[cfg(target_os = "macos")]
use std::path::Path;
use std::{collections::BTreeMap, sync::Arc};

pub struct NativeFactory;

fn sandbox_problem(_providers: &ProviderDomain) -> Result<Value> {
    #[cfg(target_os = "linux")]
    {
        Ok(if crate::provider::find_executable("bwrap").is_none() {
            json!("Discovery requires bubblewrap on Linux.")
        } else {
            Value::Null
        })
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(if !Path::new("/usr/bin/sandbox-exec").is_file() {
            json!("Discovery requires sandbox-exec on macOS.")
        } else {
            Value::Null
        });
    }
    #[cfg(windows)]
    {
        Ok(if !_providers.get("codex")?.available {
            json!(
                "Discovery requires Codex with elevated sandbox and unique permission profiles on Windows."
            )
        } else {
            Value::Null
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = _providers;
        Ok(json!(
            "Discovery isolation is unavailable on this platform."
        ))
    }
}

fn omnigraph_bootstrap_summary(
    omnigraph: &crate::omnigraph::OmnigraphManager,
    initial_omnigraph: &Value,
) -> Value {
    let mut configured = false;
    let mut mode = "off".to_owned();
    let cfg_path = omnigraph.config_path();
    if let Ok(bytes) = std::fs::read(&cfg_path)
        && let Ok(cfg) = serde_json::from_slice::<crate::omnigraph::OmnigraphConfig>(&bytes)
    {
        mode = cfg.mode.as_str().to_owned();
        configured = cfg.mode != crate::omnigraph::OmnigraphMode::Off;
    }
    let mut summary = initial_omnigraph.clone();
    summary["configured"] = json!(configured);
    summary["mode"] = json!(mode);
    summary["config_path"] = json!(cfg_path);
    summary["api_contract"] = json!(crate::omnigraph::client::HTTP_API_CONTRACT_VERSION);
    summary
}

fn bootstrap(
    store: &SessionStore,
    providers: &ProviderDomain,
    omnigraph: &crate::omnigraph::OmnigraphManager,
    initial_omnigraph: &Value,
    timeout: u64,
    concurrency: usize,
) -> Result<Value> {
    Ok(json!({
        "projects": store.projects()?,
        "providers": providers.listing()?,
        "sessions": store.summaries()?,
        "omnigraph": omnigraph_bootstrap_summary(omnigraph, initial_omnigraph),
        "workflows": [
            {"id": "native", "name": "Workspace", "description": "Use the native agent and the project's own instructions."},
            {"id": "sdd", "name": "SDD · Spec-Driven Development", "description": "Specify, plan, break down tasks, implement and review one phase at a time."},
            {"id": "plan", "name": "Plan", "description": "Inspect the project and prepare a concrete implementation plan."},
            {"id": "review", "name": "Review", "description": "Review the requested scope and report actionable findings."},
            {"id": "fleet-review", "name": "Fleet review", "description": "Run reviewers, inspect findings and approve a saved report."}
        ],
        "sdd_phases": [
            {"id": "initialize", "name": "Initialize", "description": "Create missing project principles and SDD guidance."},
            {"id": "specify", "name": "Specify", "description": "Write the feature specification: what and why."},
            {"id": "plan", "name": "Plan", "description": "Translate the reviewed specification into a technical plan."},
            {"id": "tasks", "name": "Tasks", "description": "Break the reviewed plan into test-first tasks and checkpoints."},
            {"id": "implement", "name": "Implement / resume", "description": "Execute the next task group and stop at its checkpoint."},
            {"id": "review", "name": "Review", "description": "Check implementation against acceptance criteria without editing files."}
        ],
        "clash": {
            "workflows": ["native", "review"],
            "stages": {
                "implement": "The first participant implements, the challenger attacks the change, and the implementer fixes or rebuts objections.",
                "review": "The first participant reviews, the challenger reviews independently and disputes findings, and the reviewer defends or withdraws each one."
            },
            "max_rounds": 3,
            "default_rounds": 2
        },
        "runtime": {
            "version": env!("CARGO_PKG_VERSION"),
            "timeout_seconds": timeout,
            "max_active": concurrency,
            "discovery_sandbox": sandbox_problem(providers)?,
            "max_agents": 40,
            "default_agent_count": 3,
            "context_excerpt_bytes": 3000,
            "working_memory": true,
            "fleet": {
                "available": true,
                "backend": "native",
                "max_worker_timeout": 3600,
                "lenses": [
                    {"id": "code-reviewer", "name": "Code reviewer"},
                    {"id": "security-reviewer", "name": "Security reviewer"},
                    {"id": "performance-optimization", "name": "Performance optimization"}
                ],
                "detail": "Standalone durable Rust worker state and SQLite RunWorkingMemory."
            }
        }
    }))
}

impl ServicesFactory for NativeFactory {
    fn create<'a>(
        &'a self,
        config: &'a ServerConfig,
        _control: Arc<ServerControl>,
    ) -> FactoryFuture<'a> {
        Box::pin(async move {
            let store = SessionStore::open(&config.state)?;
            for project in &config.projects {
                store.register(project)?;
            }
            let providers = Arc::new(ProviderDomain::new(&config.providers).await?);
            let links = Arc::new(SessionLinks::new());
            let graphs = crate::graph::Graphs::new(store.clone());
            let code_graph_bridge = crate::system_graph::HarnessCodeGraphBridge::new(
                store.clone(),
                graphs.clone(),
            );
            let blackboards = Arc::new(LocalBlackboards {
                store: store.clone(),
            });
            let mut special: BTreeMap<String, Arc<dyn SpecialWorkflow>> = BTreeMap::new();
            special.insert(
                "system_run".into(),
                Arc::new(SystemWorkflow {
                    store: store.clone(),
                    links: links.clone(),
                    discovery: false,
                    bridge: Some(code_graph_bridge.clone()),
                }),
            );
            special.insert(
                "system_discovery".into(),
                Arc::new(SystemWorkflow {
                    store: store.clone(),
                    links: links.clone(),
                    discovery: true,
                    bridge: None,
                }),
            );
            let sessions = Sessions::new(
                store.clone(),
                config.source.clone(),
                providers.clone(),
                blackboards,
                special,
                SessionLimits {
                    seconds: config.timeout,
                    concurrency: config.session_concurrency,
                },
            )?;
            links.bind(Arc::downgrade(&sessions))?;
            let commands = Arc::new(crate::commands::CommandCatalog::new(
                store.clone(),
                config.source.clone(),
                providers.providers.clone(),
            ));
            sessions.install_commands(commands.clone())?;
            sessions.install_graphs(graphs.clone())?;
            let backend = Arc::new(ResultsBridge {
                links: links.clone(),
                operation: sessions.operation_lock(),
                cancel: sessions.shutdown_cancel(),
            });
            let results = Arc::new(crate::results::Results::new(store.clone(), backend)?);
            sessions.install_results(results.clone())?;
            let omnigraph = Arc::new(crate::omnigraph::OmnigraphManager::new(
                store.state.clone(),
                Some(store.clone()),
            )?);
            let omnigraph_domain =
                Arc::new(crate::omnigraph::OmnigraphDomain::new(omnigraph.clone()));
            sessions.install_omnigraph(omnigraph.clone())?;
            results.install_omnigraph(omnigraph.clone())?;
            let delivery =
                Arc::new(crate::delivery::Delivery::open(store.clone(), results.clone()).await?);
            let target: Arc<dyn crate::results::TargetCheckWorkflow> = delivery.clone();
            results.bind_target(Arc::downgrade(&target))?;
            let system_bridge = Arc::new(SystemBridge {
                links: links.clone(),
                store: store.clone(),
            });
            let system_manager = Arc::new(orch_system::SystemManager::new(
                config.source.clone(),
                store.state.clone(),
                system_bridge,
            )?);
            system_manager.set_code_graph_bridge(code_graph_bridge.clone())?;
            let system_graphs = Arc::new(crate::system_graph::SystemGraphDomain::new(
                store.clone(),
                graphs.clone(),
                code_graph_bridge,
                Some(system_manager.clone()),
            ));
            let systems = Arc::new(SystemsDomain::new(system_manager));
            let project = Arc::new(crate::projects::ProjectDomain {
                store: store.clone(),
            });
            let working_memory = Arc::new(WorkingMemoryDomain::new(store.clone()));
            crate::chat_merge::ChatMerge::new(store.clone()).recover()?;
            let mut domains: BTreeMap<String, Arc<dyn Domain>> = BTreeMap::new();
            domains.insert("sessions".into(), sessions.clone());
            domains.insert("systems".into(), systems.clone());
            domains.insert("discovery".into(), systems.clone());
            domains.insert(
                "results".into(),
                Arc::new(crate::results::ResultService { results }),
            );
            domains.insert("delivery".into(), delivery);
            domains.insert("projects".into(), project.clone());
            domains.insert("files".into(), project);
            domains.insert("commands".into(), commands);
            domains.insert("providers".into(), providers.clone());
            domains.insert("working_memory".into(), working_memory);
            domains.insert("graph".into(), graphs.clone());
            domains.insert("system_graph".into(), system_graphs);
            domains.insert("omnigraph".into(), omnigraph_domain);
            let initial_omnigraph =
                tokio::time::timeout(std::time::Duration::from_secs(2), omnigraph.status_json(""))
                    .await
                    .unwrap_or_else(|_| {
                        Ok(json!({
                            "ok": false,
                            "reachable": false,
                            "status": "offline",
                            "api_contract": crate::omnigraph::client::HTTP_API_CONTRACT_VERSION,
                        }))
                    })
                    .unwrap_or_else(|e| json!({"ok": false, "reachable": false, "error": e.message}));
            let bs = store.clone();
            let bp = providers.clone();
            let bo = omnigraph.clone();
            let bio = initial_omnigraph;
            let timeout = config.timeout;
            let concurrency = config.session_concurrency;
            let bootstrap = Arc::new(move || bootstrap(&bs, &bp, &bo, &bio, timeout, concurrency));
            let closing = sessions.clone();
            let closer = Arc::new(move || {
                let closing = closing.clone();
                let systems = systems.clone();
                let graphs = graphs.clone();
                let omnigraph = omnigraph.clone();
                Box::pin(async move {
                    let requested = closing.close();
                    systems.close().await;
                    graphs.close();
                    let _ = omnigraph.close().await;
                    let reaped = closing.wait_closed().await;
                    requested.and(reaped)
                }) as crate::services::CloseFuture<'static>
            });
            let services = Arc::new(NativeServices::new(
                store,
                domains,
                bootstrap,
                BTreeMap::new(),
                closer,
            )?);
            sessions.activate()?;
            Ok(services as Arc<dyn Services>)
        })
    }
}
