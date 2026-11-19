//! Standalone generation-owned run lifecycle, accounting, and observer bindings.
use crate::{
    HarnessError, Result, launch_accounting::LaunchAccounting, sessions::Event, store::SessionStore,
};
use orch_host::{
    process::RunObserver,
    provider::{Launch, RunEvents, WorkerResult},
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Clone, Copy, Debug)]
pub struct LifecycleOptions {
    pub native_command: bool,
    pub supported: bool,
}

impl Default for LifecycleOptions {
    fn default() -> Self {
        Self {
            native_command: false,
            supported: true,
        }
    }
}

#[derive(Clone)]
pub struct RunnerLifecycle {
    pub store: SessionStore,
    pub source: PathBuf,
}

#[derive(Clone)]
pub struct RunBinding {
    pub session: Value,
    pub ready: bool,
    pub instructions: String,
    pub environment: BTreeMap<String, Option<String>>,
    state: Arc<Mutex<RunState>>,
}

struct RunState {
    store: SessionStore,
    sid: String,
    generation: String,
    cancel: Arc<AtomicBool>,
    enabled: bool,
    finalized: bool,
    started: Option<Instant>,
    launch: Option<Launch>,
    native_id: Option<String>,
    tokens: Option<u64>,
    cost: Option<f64>,
    cost_unknown: bool,
    accounting: LaunchAccounting,
    cleanup_confirmed: bool,
}

pub struct RecoveryRequest {
    pub launch: Launch,
    pub timeout: std::time::Duration,
    pub token_limit: Option<u64>,
    pub output_limit: usize,
    pub extra_env: BTreeMap<String, Option<String>>,
    pub cancel: Arc<AtomicBool>,
    pub previous_total: Option<f64>,
    pub runner_lock: Arc<std::fs::File>,
}

pub type RecoveryReply<'a> = Pin<Box<dyn Future<Output = Result<WorkerResult>> + Send + 'a>>;

pub trait RecoveryRunner: Send {
    fn run<'a>(
        &'a mut self,
        request: RecoveryRequest,
        events: Arc<dyn RunEvents>,
        emit: Event,
    ) -> RecoveryReply<'a>;
}

fn locked(binding: &RunBinding) -> Result<std::sync::MutexGuard<'_, RunState>> {
    binding
        .state
        .lock()
        .map_err(|_| HarnessError::unavailable("Launch lifecycle lock unavailable."))
}

fn current(store: &SessionStore, sid: &str, generation: &str, cancel: &AtomicBool) -> Result<Value> {
    if cancel.load(Ordering::Acquire) || store.generation(sid)? != generation {
        return Err(HarnessError::conflict(
            "The session generation is no longer active.",
        ));
    }
    let session = store.get(sid)?;
    if session["status"] == "cancelled" {
        return Err(HarnessError::conflict(
            "The session generation was cancelled.",
        ));
    }
    Ok(session)
}

fn patch(
    store: &SessionStore,
    sid: &str,
    generation: &str,
    cancel: &AtomicBool,
    fields: &Value,
) -> Result<Value> {
    store.patch_guarded(sid, generation, fields, cancel)
}

fn emit_event(store: &SessionStore, sid: &str, generation: &str, value: Value) -> Result<()> {
    if !store.guarded_event(sid, generation, &value)? {
        return Err(HarnessError::conflict(
            "The session generation is no longer active.",
        ));
    }
    Ok(())
}

impl RunnerLifecycle {
    pub fn new(store: SessionStore, source: PathBuf) -> Self {
        Self { store, source }
    }

    pub fn begin(
        &self,
        session: &Value,
        prompt: &str,
        generation: &str,
        cancel: Arc<AtomicBool>,
    ) -> Result<RunBinding> {
        self.begin_with_options(session, prompt, generation, cancel, LifecycleOptions::default())
    }

    pub fn begin_with_options(
        &self,
        session: &Value,
        _prompt: &str,
        generation: &str,
        cancel: Arc<AtomicBool>,
        options: LifecycleOptions,
    ) -> Result<RunBinding> {
        let sid = session["id"]
            .as_str()
            .ok_or_else(|| HarnessError::invalid("Session identity missing."))?
            .to_string();
        let provider = session["provider"].as_str().unwrap_or("").to_string();
        let project = PathBuf::from(session["project_path"].as_str().unwrap_or("."));
        let accounting = LaunchAccounting::new(
            &provider,
            session,
            generation,
            &project,
            &self.source,
            None,
        );
        let state = RunState {
            store: self.store.clone(),
            sid,
            generation: generation.into(),
            cancel,
            enabled: options.supported,
            finalized: false,
            started: Some(Instant::now()),
            launch: None,
            native_id: session["native_session_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            tokens: None,
            cost: None,
            cost_unknown: false,
            accounting,
            cleanup_confirmed: false,
        };
        Ok(RunBinding {
            session: session.clone(),
            ready: true,
            instructions: String::new(),
            environment: BTreeMap::new(),
            state: Arc::new(Mutex::new(state)),
        })
    }

    pub fn set_prompt_ledger(&self, binding: &RunBinding, ledger: &Value, agents: u32) -> Result<()> {
        locked(binding)?.accounting.set_ledger(ledger, agents);
        Ok(())
    }

    pub fn set_window(&self, binding: &RunBinding, window: Option<u64>) -> Result<()> {
        locked(binding)?.accounting.set_window(window);
        Ok(())
    }

    pub fn before_dispatch(&self, binding: &RunBinding, launch: &Launch) -> Result<()> {
        let mut state = locked(binding)?;
        current(&state.store, &state.sid, &state.generation, &state.cancel)?;
        if state.finalized || !binding.ready || state.launch.is_some() {
            return Err(HarnessError::conflict(
                "This launch lifecycle cannot start again.",
            ));
        }
        state.launch = Some(launch.clone());
        state.accounting.new_phase("main", launch.session.as_deref());
        state.accounting.payload("main_prompt", &launch.prompt);
        Ok(())
    }

    pub fn observe(
        &self,
        binding: &RunBinding,
        raw: Option<&Value>,
        normalized: Option<&Value>,
    ) -> Result<()> {
        observe_binding(binding, raw, normalized)
    }

    pub fn boundary(&self, binding: &RunBinding, signal: &str, details: &Value) -> Result<()> {
        boundary_binding(binding, signal, details)
    }

    pub fn abort(&self, binding: &RunBinding, outcome: &str) -> Result<Value> {
        finish_accounting(binding, outcome)
    }

    pub async fn finish(
        &self,
        binding: &RunBinding,
        result: &WorkerResult,
        _emit: &Event,
        _recovery: &mut dyn RecoveryRunner,
    ) -> Result<Value> {
        let outcome = {
            let state = locked(binding)?;
            if state.store.generation(&state.sid).ok().as_deref() != Some(&state.generation) {
                "superseded"
            } else if state.cancel.load(Ordering::Acquire) {
                "cancelled"
            } else if result.ok {
                "completed"
            } else {
                "failed"
            }
        };
        let full_cost = finish_accounting(binding, outcome)?;
        Ok(json!({
            "memory": { "state": "not-requested", "ok": null },
            "full_cost": full_cost,
        }))
    }
}

fn boundary_binding(binding: &RunBinding, signal: &str, details: &Value) -> Result<()> {
    let mut state = locked(binding)?;
    if state.finalized && signal != "cleanup_confirmed" {
        return Ok(());
    }
    let phase = if state.accounting.context().is_some() {
        "main"
    } else {
        "setup"
    };
    state.accounting.boundary(phase, signal, details);
    if signal == "cleanup_confirmed" {
        state.cleanup_confirmed = true;
    }
    Ok(())
}

fn observe_binding(
    binding: &RunBinding,
    raw: Option<&Value>,
    normalized: Option<&Value>,
) -> Result<()> {
    let mut state = locked(binding)?;
    if state.finalized {
        return Ok(());
    }
    current(&state.store, &state.sid, &state.generation, &state.cancel)?;
    if let Some(raw) = raw {
        state.accounting.raw("main", raw);
    }
    if !state.enabled {
        return Ok(());
    }
    if let Some(clean) = normalized {
        if let Some(native) = clean["native_session_id"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            if state
                .native_id
                .as_deref()
                .is_some_and(|expected| expected != native)
            {
                state.accounting.boundary("main", "identity_changed", &Value::Null);
                return Err(HarnessError::conflict(
                    "Provider changed the original session identity.",
                ));
            }
            state.native_id = Some(native.into());
        }
        if clean["kind"] == "usage" {
            if let Some(tokens) = orch_host::provider::total_tokens(clean) {
                state.tokens = Some(state.tokens.unwrap_or(0).saturating_add(tokens));
            }
            let own = clean["cost_usd"]
                .as_f64()
                .filter(|c| c.is_finite() && *c >= 0.0);
            let reported = clean["cost_usd_reported"]
                .as_f64()
                .filter(|c| c.is_finite() && *c >= 0.0);
            state.accounting.usd("main", own, reported);
            if let Some(cost) = own
                && !state.cost_unknown
            {
                state.cost = Some(state.cost.unwrap_or(0.0) + cost);
            }
            if let Some(total) = clean["cost_session_total"]
                .as_f64()
                .filter(|c| c.is_finite() && *c >= 0.0)
                && let Some(native) = state.native_id.clone()
            {
                let mut session = current(&state.store, &state.sid, &state.generation, &state.cancel)?;
                if !session["cost_totals"].is_object() {
                    session["cost_totals"] = json!({});
                }
                session["cost_totals"][native] = json!(total);
                patch(
                    &state.store,
                    &state.sid,
                    &state.generation,
                    &state.cancel,
                    &json!({"cost_totals": session["cost_totals"]}),
                )?;
            }
        }
    }
    Ok(())
}

fn finish_accounting(binding: &RunBinding, outcome: &str) -> Result<Value> {
    let mut state = locked(binding)?;
    if state.finalized {
        return Ok(state.accounting.snapshot());
    }
    state.finalized = true;
    if let Some(started) = state.started {
        state.accounting.duration("main", started.elapsed().as_secs_f64());
    }
    let snapshot = state.accounting.finish(outcome);
    let db = state.store.connection()?;
    let row = db
        .query_row(
            "SELECT session_id, receipt FROM launches WHERE id=?1",
            [&state.generation],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    if let Some((owner, raw)) = row
        && owner == state.sid
    {
        let mut receipt = raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(Value::is_object)
            .unwrap_or(json!({}));
        receipt["full_cost"] = snapshot.clone();
        db.execute(
            "UPDATE launches SET receipt=?1 WHERE id=?2 AND session_id=?3",
            params![serde_json::to_string(&receipt)?, state.generation, state.sid],
        )?;
    }
    Ok(snapshot)
}

impl RunObserver for RunBinding {
    fn boundary(&self, signal: &str, details: &Value) -> Result<()> {
        boundary_binding(self, signal, details)
    }
}

impl RunEvents for RunBinding {
    fn observe(&self, raw: Option<&Value>, normalized: Option<&Value>) -> Result<()> {
        observe_binding(self, raw, normalized)
    }

    fn enrich(&self, clean: &Value) -> Result<Value> {
        let state = locked(binding_from_self(self))?;
        if !state.enabled {
            return Ok(clean.clone());
        }
        let mut enriched = clean.clone();
        if let Some(display) = state.accounting.clean("main", clean)
            && let (Some(target), Some(display)) = (enriched.as_object_mut(), display.as_object())
        {
            target.extend(display.clone());
        }
        for notice in state.accounting.take_events() {
            emit_event(&state.store, &state.sid, &state.generation, notice)?;
        }
        Ok(enriched)
    }
}

fn binding_from_self(b: &RunBinding) -> &RunBinding {
    b
}
