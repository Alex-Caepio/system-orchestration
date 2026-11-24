//! Concrete bridges share the Harness queue and registered project boundaries.
use crate::{
    HarnessError, Result,
    services::{Domain, Payload, Reply},
    sessions::{Event, RunReply, Sessions, SpecialWorkflow},
    store::SessionStore,
};
use orch_core::RuntimeError;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, OnceLock, Weak, atomic::AtomicBool},
};
pub struct SessionLinks {
    runner: OnceLock<Weak<Sessions>>,
}
impl SessionLinks {
    pub fn new() -> Self {
        Self {
            runner: OnceLock::new(),
        }
    }
    pub fn bind(&self, runner: Weak<Sessions>) -> Result<()> {
        self.runner
            .set(runner)
            .map_err(|_| HarnessError::conflict("The native session bridge was already bound."))
    }
    pub fn runner(&self) -> Result<Arc<Sessions>> {
        self.runner
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| HarnessError::unavailable("The session runner is not available."))
    }
}

impl Default for SessionLinks {
    fn default() -> Self {
        Self::new()
    }
}
fn core(error: HarnessError) -> RuntimeError {
    match error.status {
        400 => RuntimeError::invalid(error.message),
        403 => RuntimeError::policy(error.message),
        404 => RuntimeError::not_found(error.message),
        409 => RuntimeError::conflict(error.message),
        _ => RuntimeError::unavailable(error.message),
    }
}
pub struct SystemBridge {
    pub links: Arc<SessionLinks>,
    pub store: SessionStore,
}
impl orch_system::SessionBridge for SystemBridge {
    fn projects(&self) -> orch_core::Result<BTreeMap<String, orch_system::Project>> {
        self.store
            .projects()
            .map_err(core)?
            .into_iter()
            .map(|p| {
                let id = p["id"]
                    .as_str()
                    .ok_or_else(|| RuntimeError::invalid("Project identity missing."))?
                    .to_owned();
                Ok((
                    id.clone(),
                    orch_system::Project {
                        id,
                        name: p["name"].as_str().unwrap_or("").into(),
                        path: PathBuf::from(
                            p["path"]
                                .as_str()
                                .ok_or_else(|| RuntimeError::invalid("Project path missing."))?,
                        ),
                    },
                ))
            })
            .collect()
    }
    fn provider(&self, id: &str) -> orch_core::Result<Option<PathBuf>> {
        Ok(self
            .links
            .runner()
            .map_err(core)?
            .providers
            .get(id)
            .map_err(core)?
            .executable)
    }
    fn active(&self) -> orch_core::Result<bool> {
        let db = self.store.connection().map_err(core)?;
        Ok(db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE status IN ('queued','running'))",
            [],
            |r| r.get(0),
        )?)
    }
    fn admit(&self) -> orch_core::Result<()> {
        self.links.runner().map_err(core)?.admit().map_err(core)
    }
    fn runner_lock(&self) -> orch_core::Result<Arc<std::fs::File>> {
        Ok(self.store.runner_lock())
    }
    fn launch(
        &self,
        request: &Value,
        system: Option<&Value>,
        discovery: Option<&Value>,
    ) -> orch_core::Result<Value> {
        self.links
            .runner()
            .map_err(core)?
            .launch_internal(request, system, discovery)
            .map_err(core)
    }
    fn session(&self, id: &str) -> orch_core::Result<Value> {
        self.store.get(id).map_err(core)
    }
    fn cancel(&self, id: &str) -> orch_core::Result<Value> {
        self.links.runner().map_err(core)?.cancel(id).map_err(core)
    }
    fn events(&self, id: &str, kinds: &[&str]) -> orch_core::Result<Vec<Value>> {
        let mut out = vec![];
        let mut cursor = 0i64;
        loop {
            let batch = self.store.events(id, cursor).map_err(core)?;
            if batch.is_empty() {
                break;
            }
            for event in batch {
                cursor = event["id"].as_i64().unwrap_or(cursor);
                if kinds.is_empty() || kinds.contains(&event["kind"].as_str().unwrap_or("")) {
                    out.push(event);
                }
            }
            if out.len() > 10000 {
                return Err(RuntimeError::invalid(
                    "System event view exceeds its item bound.",
                ));
            }
        }
        Ok(out)
    }
    fn waiting_permissions(&self, id: &str) -> orch_core::Result<Vec<Value>> {
        Ok(self.links.runner().map_err(core)?.waiting_permissions(id))
    }
    fn find_launch(&self, run_id: &str, nonce: &str) -> orch_core::Result<Option<Value>> {
        let rows = {
            let db = self.store.connection().map_err(core)?;
            let mut q=db.prepare("SELECT id,system_run,system_discovery FROM sessions WHERE system_run IS NOT NULL OR system_discovery IS NOT NULL ORDER BY rowid DESC LIMIT 100000")?;
            q.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (id, system, discovery) in rows {
            for raw in [system, discovery].into_iter().flatten() {
                let metadata = crate::strict_json::parse(raw.as_bytes(), 65536).map_err(core)?;
                if metadata["run_id"] == run_id && metadata["nonce"] == nonce {
                    return self.store.get(&id).map(Some).map_err(core);
                }
            }
        }
        Ok(None)
    }
}
pub struct ResultsBridge {
    pub links: Arc<SessionLinks>,
    pub operation: Arc<tokio::sync::Mutex<()>>,
    pub cancel: Arc<AtomicBool>,
}
impl crate::results::ResultsBackend for ResultsBridge {
    fn workspace(&self, session: &Value) -> Result<PathBuf> {
        let path = PathBuf::from(
            session["project_path"]
                .as_str()
                .ok_or_else(|| HarnessError::invalid("Session workspace missing."))?,
        );
        let canonical = orch_core::fs::canonicalize(&path)?;
        if canonical != path {
            return Err(HarnessError::forbidden("Session workspace changed."));
        }
        Ok(path)
    }
    fn idle(&self) -> Result<()> {
        self.links.runner()?.idle()
    }
    fn enqueue_check(&self, sid: &str, check_id: &str, previous_status: &str) -> Result<String> {
        self.links
            .runner()?
            .enqueue_check(sid, check_id, previous_status)
    }
    fn operation_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.operation.clone()
    }
    fn history_session_ids(&self, session: &Value) -> Result<Vec<String>> {
        self.links.runner()?.history_session_ids(session)
    }
    fn shutdown_cancel(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
}
pub struct SystemsDomain {
    pub manager: Arc<orch_system::SystemManager>,
    // One owned guard covers both submitted and running blocking work. The
    // flag closes admission after the last preceding operation has drained.
    pub(crate) gate: Arc<tokio::sync::Mutex<bool>>,
}
impl SystemsDomain {
    pub fn new(manager: Arc<orch_system::SystemManager>) -> Self {
        Self {
            manager,
            gate: Arc::new(tokio::sync::Mutex::new(false)),
        }
    }
    pub async fn close(&self) {
        *self.gate.lock().await = true;
    }
}
impl Domain for SystemsDomain {
    fn execute<'a>(&'a self, op: &'a str, request: Value) -> Reply<'a> {
        Box::pin(async move {
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                let gate = self.gate.blocking_lock();
                if *gate {
                    return Err(HarnessError::conflict("The system manager is closed."));
                }
                return Ok(Payload::Json(self.manager.execute(op, &request)?));
            };
            let gate = self.gate.clone().lock_owned().await;
            if *gate {
                return Err(HarnessError::conflict("The system manager is closed."));
            }
            let manager = self.manager.clone();
            let operation = op.to_owned();
            runtime
                .spawn_blocking(move || {
                    // Dropping the caller must not admit another operation
                    // until this synchronous custody boundary has finished.
                    let _gate = gate;
                    Ok(Payload::Json(manager.execute(&operation, &request)?))
                })
                .await
                .map_err(|_| HarnessError::unavailable("System operation worker stopped."))?
        })
    }
}
struct WorkflowObserver {
    store: SessionStore,
    sid: String,
    generation: String,
    cancel: Arc<AtomicBool>,
}
impl orch_host::process::RunObserver for WorkflowObserver {
    fn boundary(&self, signal: &str, details: &Value) -> Result<()> {
        if self.cancel.load(std::sync::atomic::Ordering::Acquire) && signal != "cleanup_confirmed" {
            return Err(HarnessError::conflict("Cancellation requested."));
        }
        if !self.store.guarded_event(
            &self.sid,
            &self.generation,
            &json!({"kind":"process_boundary","signal":signal,"details":details}),
        )? {
            return Err(HarnessError::conflict(
                "The system session generation changed.",
            ));
        }
        Ok(())
    }
}
pub struct SystemWorkflow {
    pub store: SessionStore,
    pub links: Arc<SessionLinks>,
    pub discovery: bool,
    pub bridge: Option<Arc<crate::system_graph::HarnessCodeGraphBridge>>,
}
impl SpecialWorkflow for SystemWorkflow {
    fn run<'a>(
        &'a self,
        session: Value,
        _prompt: String,
        generation: String,
        cancel: Arc<AtomicBool>,
        emit: Event,
    ) -> RunReply<'a> {
        Box::pin(async move {
            let metadata = &session[if self.discovery {
                "system_discovery"
            } else {
                "system_run"
            }];
            let run = metadata["run_id"]
                .as_str()
                .ok_or_else(|| HarnessError::invalid("System run identity missing."))?
                .to_owned();
            let nonce = metadata["nonce"]
                .as_str()
                .ok_or_else(|| HarnessError::invalid("System nonce missing."))?;
            if [run.as_str(), nonce].iter().any(|s| {
                s.len() != 32
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            }) {
                return Err(HarnessError::forbidden("Invalid internal system request."));
            }
            let directory = self
                .store
                .state
                .join(if self.discovery {
                    "system-discovery"
                } else {
                    "ai-system"
                })
                .join(&run);
            let name = format!("request-{nonce}.json");
            {
                let path = directory.join(&name);
                orch_core::fs::admit_descendant(&directory, &path)?;
                orch_core::fs::read_regular(&path)?;
            }
            let adapter: Arc<dyn orch_host::provider::LaunchAdapter> = if self.discovery {
                Arc::new(crate::launch_adapter::EvidenceLaunchAdapter {
                    workspace: directory.join("agent"),
                })
            } else {
                self.links.runner()?.system_launch_adapter()?
            };
            let sid = session["id"].as_str().unwrap_or("").to_owned();
            let approvals = (!self.discovery)
                .then(|| {
                    self.links
                        .runner()
                        .ok()
                        .and_then(|runner| runner.system_approvals(&sid, &session, &emit))
                })
                .flatten();
            let observer = Arc::new(WorkflowObserver {
                store: self.store.clone(),
                sid: sid.clone(),
                generation: generation.clone(),
                cancel: cancel.clone(),
            });
            let lock = Some(self.store.runner_lock());
            let display = emit.clone();
            let activity_store = self.store.clone();
            let activity_sid = sid.clone();
            let activity_gen = generation.clone();
            let activity_bridge = (!self.discovery).then(|| self.bridge.clone()).flatten();
            let activity_run = run.clone();
            let mut activity: Option<crate::agent_activity::ActivityStream> = None;
            let agent =
                move |signal: orch_system::workers::AgentSignal| -> orch_core::Result<()> {
                    use orch_system::workers::AgentSignal;
                    match signal {
                        AgentSignal::Configured {
                            provider,
                            roots,
                            aliases,
                        } => {
                            let roots = roots
                                .into_iter()
                                .map(|(id, path)| (id, path.to_string_lossy().into_owned()))
                                .collect();
                            let aliases = aliases
                                .into_iter()
                                .map(|(path, value)| (path.to_string_lossy().into_owned(), value))
                                .collect();
                            if let Some(bridge) = activity_bridge.clone() {
                                let store = activity_store.clone();
                                let sid = activity_sid.clone();
                                let generation = activity_gen.clone();
                                let run_id = activity_run.clone();
                                activity =
                                    Some(crate::agent_activity::ActivityStream::new_with_ids(
                                        move |event| {
                                            let Some(id) = store.guarded_event_id(
                                                &sid,
                                                &generation,
                                                &event,
                                            )?
                                            else {
                                                return Err(HarnessError::conflict(
                                                    "The session generation changed.",
                                                ));
                                            };
                                            Ok(Some(id))
                                        },
                                        move |event_id, event| {
                                            bridge
                                                .on_emitted_session_event(&run_id, event_id, event);
                                        },
                                        &provider,
                                        roots,
                                        aliases,
                                    ));
                            } else {
                                let display = display.clone();
                                activity = Some(crate::agent_activity::ActivityStream::new(
                                    move |event| display(event),
                                    &provider,
                                    roots,
                                    aliases,
                                ));
                            }
                        }
                        AgentSignal::Started {
                            agent,
                            attempt,
                            cwd,
                            details,
                        } => {
                            if let Some(activity) = &mut activity {
                                activity.start(&agent, attempt, &cwd.to_string_lossy(), &details);
                            }
                        }
                        AgentSignal::Output { bytes } => {
                            if let Some(activity) = &mut activity {
                                activity.line(&bytes);
                            }
                        }
                        AgentSignal::Finished { status, details } => {
                            if let Some(activity) = &mut activity {
                                activity.finish(&status, &details);
                            }
                        }
                    }
                    Ok(())
                };
            let report = if self.discovery {
                orch_system::workers::run_discovery_request_with_adapter(
                    &directory.join(name),
                    cancel,
                    lock,
                    adapter,
                    observer,
                    move |event| emit(event).map_err(core),
                    agent,
                )
                .await?
            } else if let Some(bridge) = &self.bridge {
                struct SessionGuard<'a> {
                    bridge: &'a crate::system_graph::HarnessCodeGraphBridge,
                    run_id: String,
                }
                impl Drop for SessionGuard<'_> {
                    fn drop(&mut self) {
                        self.bridge.unregister_session(&self.run_id);
                    }
                }
                bridge.register_session(&run, &sid, &generation);
                let _session_guard = SessionGuard {
                    bridge,
                    run_id: run.clone(),
                };
                orch_system::workers::run_system_request_with_bridge_and_approvals(
                    &directory.join(name),
                    cancel,
                    lock,
                    adapter,
                    observer,
                    bridge.clone(),
                    Some(sid),
                    approvals,
                    move |event| emit(event).map_err(core),
                    agent,
                )
                .await?
            } else {
                orch_system::workers::run_system_request_with_adapter_and_approvals(
                    &directory.join(name),
                    cancel,
                    lock,
                    adapter,
                    observer,
                    approvals,
                    move |event| emit(event).map_err(core),
                    agent,
                )
                .await?
            };
            Ok(report)
        })
    }
}

#[cfg(test)]
#[path = "bindings_tests.rs"]
mod tests;
