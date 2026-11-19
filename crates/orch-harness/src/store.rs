use crate::{HarnessError, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
#[derive(Clone)]
pub struct SessionStore {
    pub state: PathBuf,
    db: Arc<Mutex<Connection>>,
    _runner: Arc<File>,
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn hexid() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
pub fn sha(bytes: &[u8]) -> String {
    orch_core::query::sha256(bytes)
}
pub fn exact(value: &Value, required: &[&str], optional: &[&str]) -> bool {
    value.as_object().is_some_and(|m| {
        required.iter().all(|k| m.contains_key(*k))
            && m.keys()
                .all(|k| required.contains(&k.as_str()) || optional.contains(&k.as_str()))
    })
}
pub fn text(value: &Value, name: &str, max: usize) -> Result<String> {
    let s = value
        .as_str()
        .ok_or_else(|| HarnessError::invalid(format!("{name} must be text.")))?;
    if s.trim().is_empty() || s.chars().count() > max || s.contains('\0') {
        return Err(HarnessError::invalid(format!("Invalid {name}.")));
    }
    Ok(s.into())
}
fn generation_event(generation: &str, event: &Value) -> Result<String> {
    let mut event = event.clone();
    if !event.is_object() {
        return Err(HarnessError::invalid(
            "A generation event must be an object.",
        ));
    }
    if event
        .get("launch_id")
        .is_some_and(|v| v.as_str() != Some(generation))
    {
        return Err(HarnessError::conflict(
            "A generation event has a different launch identity.",
        ));
    }
    event["launch_id"] = generation.into();
    if event.get("at").is_none() {
        event["at"] = chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            .into();
    }
    let raw = event.to_string();
    if raw.len() > 4 * 1024 * 1024 {
        return Err(HarnessError::invalid("Event byte limit exceeded."));
    }
    Ok(raw)
}
fn confined(state: &Path, name: &str) -> Result<PathBuf> {
    let path = state.join(name);
    orch_core::fs::admit_descendant(state, &path)?;
    Ok(path)
}
impl SessionStore {
    pub fn open(state: &Path) -> Result<Self> {
        orch_host::util::private_directory(state)?;
        let state = orch_core::fs::python_realpath(state)?;
        let lockpath = confined(&state, "runner.lock")?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0).custom_flags(0x00200000);
        }
        let runner = options.open(lockpath)?;
        if !runner.metadata()?.is_file() {
            return Err(HarnessError::forbidden(
                "Runner lock must be a regular file.",
            ));
        }
        #[cfg(unix)]
        runner.try_lock().map_err(|_| {
            HarnessError::conflict("Another runner still owns this state directory.")
        })?;
        let database = confined(&state, "sessions.sqlite3")?;
        let db = Connection::open(database)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA foreign_keys=ON;
   CREATE TABLE IF NOT EXISTS sessions(
    id TEXT PRIMARY KEY,title TEXT NOT NULL,project_id TEXT NOT NULL,project_path TEXT NOT NULL,
    provider TEXT NOT NULL,model TEXT,mode TEXT NOT NULL,workflow TEXT NOT NULL,project_context INTEGER NOT NULL,
    status TEXT NOT NULL,native_session_id TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL,data TEXT NOT NULL);
   CREATE INDEX IF NOT EXISTS events_session ON events(session_id,id);
   CREATE TABLE IF NOT EXISTS session_merges(session_id TEXT PRIMARY KEY,request_id TEXT UNIQUE NOT NULL,request_hash TEXT NOT NULL,bundle TEXT NOT NULL,summary TEXT NOT NULL,context TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS registered_projects(id TEXT PRIMARY KEY,path TEXT NOT NULL UNIQUE);
   CREATE TABLE IF NOT EXISTS memory_result_attempts(session_id TEXT NOT NULL,generation TEXT NOT NULL,binding TEXT NOT NULL,PRIMARY KEY(session_id,generation));
   CREATE TABLE IF NOT EXISTS attachments(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,name TEXT NOT NULL,size INTEGER NOT NULL,digest TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS harness_settings(name TEXT PRIMARY KEY,value TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS native_fleet_runs(thread_id TEXT PRIMARY KEY,state TEXT NOT NULL,stage TEXT NOT NULL,decision INTEGER,import_id TEXT,revision INTEGER NOT NULL DEFAULT 1);
   CREATE TABLE IF NOT EXISTS native_fleet_tasks(thread_id TEXT NOT NULL,task_id TEXT NOT NULL,lens TEXT NOT NULL,ordinal INTEGER NOT NULL,status TEXT NOT NULL,input TEXT NOT NULL,output TEXT,error TEXT,PRIMARY KEY(thread_id,task_id));
   CREATE TABLE IF NOT EXISTS native_checkpoint_imports(import_id TEXT PRIMARY KEY,thread_id TEXT NOT NULL,checkpoint_id TEXT NOT NULL,source_digest TEXT NOT NULL,created_at TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS harness_generations(session_id TEXT PRIMARY KEY,generation TEXT NOT NULL);")?;
        let columns = {
            let mut q = db.prepare("PRAGMA table_info(sessions)")?;
            q.query_map([], |r| r.get::<_, String>(1))?
                .collect::<std::result::Result<BTreeSet<_>, _>>()?
        };
        for (name, definition) in [
            ("agents_enabled", "INTEGER NOT NULL DEFAULT 0"),
            ("agent_count", "INTEGER NOT NULL DEFAULT 3"),
            ("thinking_effort", "TEXT"),
            ("workspace", "TEXT NOT NULL DEFAULT 'project'"),
            ("branch", "TEXT"),
            ("git_common_dir", "TEXT"),
            ("fleet", "TEXT"),
            ("fleet_result", "TEXT"),
            ("fleet_action", "TEXT"),
            ("brain", "TEXT"),
            ("creator", "TEXT"),
            ("system_run", "TEXT"),
            ("system_discovery", "TEXT"),
            ("budgets", "TEXT"),
            ("budget_usage", "TEXT"),
            ("budget_revision", "INTEGER NOT NULL DEFAULT 0"),
            ("result_base", "TEXT"),
            ("sdd", "TEXT"),
            ("model_routing", "TEXT"),
            ("clash", "TEXT"),
            ("clash_result", "TEXT"),
            ("cost_totals", "TEXT"),
        ] {
            if !columns.contains(name) {
                db.execute_batch(&format!(
                    "ALTER TABLE sessions ADD COLUMN {name} {definition}"
                ))?;
            }
        }
        crate::inbox::initialize(&db)?;
        let restart = db.unchecked_transaction()?;
        let interrupted = {
            let mut q =
                restart.prepare("SELECT id FROM sessions WHERE status IN ('queued','running')")?;
            q.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for sid in interrupted {
            restart.execute("INSERT INTO events(session_id,data)VALUES(?1,?2)",params![sid,json!({"kind":"status","text":"Server restarted; the previous run was interrupted."}).to_string()])?;
        }
        restart.execute("UPDATE sessions SET status='interrupted',updated_at=?1 WHERE status IN ('queued','running')",[now()])?;
        crate::inbox::recover(&restart)?;
        restart.commit()?;
        Ok(Self {
            state,
            db: Arc::new(Mutex::new(db)),
            _runner: Arc::new(runner),
        })
    }
    pub fn runner_lock(&self) -> Arc<File> {
        self._runner.clone()
    }
    pub fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| HarnessError::unavailable("Session store lock was poisoned."))
    }
    pub fn setting(&self, name: &str) -> Result<Option<Value>> {
        let db = self.connection()?;
        let raw: Option<String> = db
            .query_row(
                "SELECT value FROM harness_settings WHERE name=?1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|s| crate::strict_json::parse(s.as_bytes(), 16 * 1024 * 1024))
            .transpose()
    }
    pub fn set_setting(&self, name: &str, value: &Value) -> Result<()> {
        self.connection()?.execute("INSERT INTO harness_settings(name,value)VALUES(?1,?2)ON CONFLICT(name)DO UPDATE SET value=excluded.value",params![name,value.to_string()])?;
        Ok(())
    }
    pub fn register(&self, path: &Path) -> Result<Value> {
        std::fs::metadata(path)?;
        let root = orch_core::fs::python_realpath(path)?;
        if !root.is_dir() {
            return Err(HarnessError::invalid(
                "Each project must be an existing directory.",
            ));
        }
        let path = root
            .to_str()
            .ok_or_else(|| HarnessError::invalid("Project path is not UTF-8."))?;
        let identity = if cfg!(windows) {
            path.replace('/', "\\").to_lowercase()
        } else {
            path.to_owned()
        };
        let key = sha(identity.as_bytes())[..16].to_string();
        self.connection()?.execute("INSERT INTO registered_projects(id,path)VALUES(?1,?2)ON CONFLICT(id)DO UPDATE SET path=excluded.path",params![key,path])?;
        Ok(
            json!({"id":key,"name":root.file_name().unwrap_or_default().to_string_lossy(),"path":path}),
        )
    }
    pub fn projects(&self) -> Result<Vec<Value>> {
        let db = self.connection()?;
        let mut q = db.prepare("SELECT id,path FROM registered_projects ORDER BY rowid")?;
        let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (key, path) = row?;
            out.push(json!({"id":key,"name":Path::new(&path).file_name().unwrap_or_default().to_string_lossy(),"path":path}));
        }
        Ok(out)
    }
    pub fn project(&self, key: &str) -> Result<Value> {
        self.projects()?
            .into_iter()
            .find(|v| v["id"] == key)
            .ok_or_else(|| HarnessError::missing("Project not found."))
    }
    pub fn get(&self, sid: &str) -> Result<Value> {
        let db = self.connection()?;
        get_session(&db, sid)
    }
    pub fn summaries(&self) -> Result<Vec<Value>> {
        let db = self.connection()?;
        let mut q = db.prepare(
            "SELECT s.*,m.summary FROM sessions s LEFT JOIN session_merges m ON m.session_id=s.id ORDER BY s.updated_at DESC,s.rowid DESC LIMIT 200",
        )?;
        let names = session_columns(&q);
        let mut rows = q.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(decode_session(row, &names)?);
        }
        Ok(out)
    }
    pub fn session_events(&self, sid: &str, after: i64) -> Result<(Value, Vec<Value>)> {
        let db = self.connection()?;
        // The HTTP route has always validated the session before rejecting a
        // negative cursor. Standalone `events` keeps its cursor-first order.
        let session = get_session(&db, sid)?;
        let events = session_events(&db, sid, after)?;
        Ok((session, events))
    }
    pub fn events(&self, sid: &str, after: i64) -> Result<Vec<Value>> {
        if after < 0 {
            return Err(HarnessError::invalid("Invalid event cursor."));
        }
        let db = self.connection()?;
        get_session(&db, sid)?;
        session_events(&db, sid, after)
    }
    pub fn event(&self, sid: &str, event: &Value) -> Result<i64> {
        if !event.is_object() {
            return Err(HarnessError::invalid("Event must be an object."));
        }
        let mut event = event.clone();
        if event.get("at").is_none() {
            event["at"] = chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                .into();
        }
        let raw = event.to_string();
        if raw.len() > 4 * 1024 * 1024 {
            return Err(HarnessError::invalid("Event byte limit exceeded."));
        }
        let db = self.connection()?;
        db.execute(
            "INSERT INTO events(session_id,data)VALUES(?1,?2)",
            params![sid, raw],
        )?;
        Ok(db.last_insert_rowid())
    }
    pub fn create(&self, options: &Value, prompt: &str, attachments: &[Value]) -> Result<Value> {
        self.create_transaction(options, prompt, attachments, None)
    }
    pub fn create_uploaded(
        &self,
        options: &Value,
        prompt: &str,
        uploads: &[crate::attachments::Upload],
    ) -> Result<Value> {
        self.create_transaction(options, prompt, &[], Some(uploads))
    }
    fn create_transaction(
        &self,
        options: &Value,
        prompt: &str,
        attachments: &[Value],
        uploads: Option<&[crate::attachments::Upload]>,
    ) -> Result<Value> {
        let project = self.project(
            options["project_id"]
                .as_str()
                .ok_or_else(|| HarnessError::invalid("Select a registered project."))?,
        )?;
        let provider = options["provider"]
            .as_str()
            .ok_or_else(|| HarnessError::invalid("Select a provider."))?;
        if !["claude", "codex", "cursor"].contains(&provider) {
            return Err(HarnessError::invalid("Unknown provider."));
        }
        let prompt = text(&json!(prompt), "prompt", 100_000)?;
        let sid = options["_session_id"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(id);
        let at = now();
        let generation = hexid();
        let mut db = self.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM sessions WHERE status='queued'",
            [],
            |r| r.get(0),
        )?;
        if count >= 16 {
            return Err(HarnessError::conflict("The run queue is full."));
        }
        let (staged, mut upload_guard) = if let Some(uploads) = uploads {
            crate::attachments::stage(&tx, &self.state, &sid, uploads)?
        } else {
            (
                attachments.to_vec(),
                crate::attachments::UploadGuard {
                    folders: vec![],
                    committed: false,
                },
            )
        };
        insert_session(
            &tx,
            options,
            &project,
            &prompt,
            &staged,
            &sid,
            &at,
            &generation,
        )?;
        for key in ["creator", "system_run", "system_discovery"] {
            if !options[key].is_null() {
                tx.execute(
                    &format!("UPDATE sessions SET {key}=?1 WHERE id=?2"),
                    params![options[key].to_string(), sid],
                )?;
            }
        }
        tx.commit()?;
        upload_guard.commit();
        drop(db);
        self.get(&sid)
    }
    pub fn generation(&self, sid: &str) -> Result<String> {
        self.connection()?
            .query_row(
                "SELECT generation FROM harness_generations WHERE session_id=?1",
                [sid],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }
    pub fn guarded_event(&self, sid: &str, generation: &str, event: &Value) -> Result<bool> {
        let mut db = self.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT generation FROM harness_generations WHERE session_id=?1",
                [sid],
                |r| r.get(0),
            )
            .optional()?;
        if current.as_deref() != Some(generation) {
            return Ok(false);
        }
        let raw = generation_event(generation, event)?;
        tx.execute(
            "INSERT INTO events(session_id,data)VALUES(?1,?2)",
            params![sid, raw],
        )?;
        tx.commit()?;
        Ok(true)
    }
    pub fn cancel_current(&self, sid: &str) -> Result<Value> {
        let mut db = self.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = get_session(&tx, sid)?;
        if !["queued", "running"].contains(&session["status"].as_str().unwrap_or("")) {
            crate::inbox::cancel_session(&tx, sid)?;
            tx.commit()?;
            return Ok(session);
        }
        if session["creator"]["phase"]
            .as_str()
            .is_some_and(|phase| ["apply", "rollback"].contains(&phase))
        {
            return Err(HarnessError::conflict(
                "Publication is completing verification or rollback.",
            ));
        }
        let generation: String = tx.query_row(
            "SELECT generation FROM harness_generations WHERE session_id=?1",
            [sid],
            |row| row.get(0),
        )?;
        let event = generation_event(
            &generation,
            &json!({"kind":"status","text":"Cancellation requested."}),
        )?;
        tx.execute("UPDATE sessions SET status='cancelled',updated_at=?1 WHERE id=?2 AND status IN ('queued','running')", params![now(),sid])?;
        crate::inbox::cancel_session(&tx, sid)?;
        tx.execute(
            "INSERT INTO events(session_id,data)VALUES(?1,?2)",
            params![sid, event],
        )?;
        let updated = get_session(&tx, sid)?;
        tx.commit()?;
        Ok(updated)
    }
    pub fn finish_generation(
        &self,
        sid: &str,
        generation: &str,
        fields: &Value,
        events: &[Value],
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<Value>> {
        use std::sync::atomic::Ordering;
        let fields = fields
            .as_object()
            .ok_or_else(|| HarnessError::invalid("Terminal session update must be an object."))?;
        if fields
            .keys()
            .any(|key| !["status", "fleet_result"].contains(&key.as_str()))
            || fields
                .get("status")
                .is_some_and(|status| status.as_str().is_none_or(|s| s.is_empty() || s.len() > 64))
        {
            return Err(HarnessError::invalid("Invalid terminal session update."));
        }
        let mut db = self.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String,String)> = tx.query_row(
            "SELECT g.generation,s.status FROM harness_generations g JOIN sessions s ON s.id=g.session_id WHERE s.id=?1",
            [sid], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        let Some((current, status)) = current else {
            return Ok(None);
        };
        if current != generation {
            return Ok(None);
        }
        // Cancellation is durable even when the queued job never acquired a
        // live token. Its old completion must neither resurrect it nor append
        // a spurious failure/check-finished event.
        if status == "cancelled" {
            return Ok(Some(get_session(&tx, sid)?));
        }
        let status = if cancel.load(Ordering::Acquire) {
            "cancelled"
        } else {
            fields
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or(&status)
        };
        if fields.contains_key("status") || status == "cancelled" {
            tx.execute(
                "UPDATE sessions SET status=?1,updated_at=?2 WHERE id=?3 AND EXISTS(SELECT 1 FROM harness_generations WHERE session_id=?3 AND generation=?4)",
                params![status,now(),sid,generation],
            )?;
        }
        if let Some(value) = fields.get("fleet_result") {
            let value = if value.is_null() {
                None
            } else {
                Some(value.to_string())
            };
            tx.execute(
                "UPDATE sessions SET fleet_result=?1,updated_at=?2 WHERE id=?3",
                params![value, now(), sid],
            )?;
        }
        for event in events {
            let mut event = event.clone();
            if event.get("status").is_some() {
                event["status"] = status.into();
            }
            let event = generation_event(generation, &event)?;
            tx.execute(
                "INSERT INTO events(session_id,data)VALUES(?1,?2)",
                params![sid, event],
            )?;
        }
        let updated = get_session(&tx, sid)?;
        tx.commit()?;
        Ok(Some(updated))
    }
    pub fn patch_guarded(
        &self,
        sid: &str,
        generation: &str,
        fields: &Value,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Value> {
        use std::sync::atomic::Ordering;
        let fields = fields
            .as_object()
            .ok_or_else(|| HarnessError::invalid("Session update must be an object."))?;
        let allowed = [
            "status",
            "native_session_id",
            "model",
            "thinking_effort",
            "mode",
            "workspace",
            "branch",
            "git_common_dir",
            "agents_enabled",
            "agent_count",
            "fleet",
            "fleet_result",
            "fleet_action",
            "brain",
            "creator",
            "system_run",
            "system_discovery",
            "budgets",
            "budget_usage",
            "budget_revision",
            "result_base",
            "sdd",
            "model_routing",
            "clash",
            "clash_result",
            "cost_totals",
        ];
        if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(HarnessError::invalid("Unknown session update."));
        }
        let mut db = self.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current:Option<(String,String)>=tx.query_row("SELECT g.generation,s.status FROM harness_generations g JOIN sessions s ON s.id=g.session_id WHERE s.id=?1",[sid],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if cancel.load(Ordering::Acquire)
            || current
                .as_ref()
                .is_none_or(|(g, status)| g != generation || status == "cancelled")
        {
            return Err(HarnessError::conflict(
                "This session generation is no longer current.",
            ));
        }
        for (key, value) in fields {
            let value = match value {
                Value::Null => rusqlite::types::Value::Null,
                Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                Value::Bool(b) => rusqlite::types::Value::Integer(i64::from(*b)),
                Value::Number(n) if n.as_i64().is_some() => {
                    rusqlite::types::Value::Integer(n.as_i64().unwrap_or_default())
                }
                _ => rusqlite::types::Value::Text(value.to_string()),
            };
            tx.execute(
                &format!("UPDATE sessions SET {key}=?1,updated_at=?2 WHERE id=?3"),
                params![value, now(), sid],
            )?;
        }
        if cancel.load(Ordering::Acquire) {
            return Err(HarnessError::conflict("Cancellation requested."));
        }
        let updated = get_session(&tx, sid)?;
        tx.commit()?;
        Ok(updated)
    }
    pub fn patch(&self, sid: &str, fields: &Value) -> Result<Value> {
        let map = fields
            .as_object()
            .ok_or_else(|| HarnessError::invalid("Session update must be an object."))?;
        let allowed = [
            "status",
            "native_session_id",
            "model",
            "thinking_effort",
            "mode",
            "workspace",
            "branch",
            "git_common_dir",
            "agents_enabled",
            "agent_count",
            "fleet",
            "fleet_result",
            "fleet_action",
            "brain",
            "creator",
            "system_run",
            "system_discovery",
            "budgets",
            "budget_usage",
            "budget_revision",
            "result_base",
            "sdd",
            "model_routing",
            "clash",
            "clash_result",
            "cost_totals",
        ];
        if map.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(HarnessError::invalid("Unknown session update."));
        }
        let mut db = self.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (key, value) in map {
            let value = match value {
                Value::Null => rusqlite::types::Value::Null,
                Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                Value::Bool(b) => rusqlite::types::Value::Integer(i64::from(*b)),
                Value::Number(n) if n.as_i64().is_some() => {
                    rusqlite::types::Value::Integer(n.as_i64().unwrap_or_default())
                }
                _ => rusqlite::types::Value::Text(value.to_string()),
            };
            tx.execute(
                &format!("UPDATE sessions SET {key}=?1,updated_at=?2 WHERE id=?3"),
                params![value, now(), sid],
            )?;
        }
        tx.commit()?;
        drop(db);
        self.get(sid)
    }
    pub fn requeue(&self, sid: &str, prompt: Option<&str>, settings: &Value) -> Result<Value> {
        self.requeue_uploaded(sid, prompt, settings, &[])
    }
    pub fn requeue_uploaded(
        &self,
        sid: &str,
        prompt: Option<&str>,
        settings: &Value,
        uploads: &[crate::attachments::Upload],
    ) -> Result<Value> {
        let old = self.get(sid)?;
        if ["running", "queued"].contains(&old["status"].as_str().unwrap_or("")) {
            return Err(HarnessError::conflict("Wait for this session to finish."));
        }
        let fields = settings
            .as_object()
            .ok_or_else(|| HarnessError::invalid("Session options must be an object."))?;
        let allowed = [
            "model",
            "thinking_effort",
            "sdd",
            "mode",
            "model_routing",
            "agents_enabled",
            "agent_count",
            "clash",
            "brain",
            "fleet_action",
        ];
        if fields.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err(HarnessError::invalid("Unknown follow-up option."));
        }
        let mut db = self.connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = get_session(&tx, sid)?;
        if ["queued", "running"].contains(&current["status"].as_str().unwrap_or("")) {
            return Err(HarnessError::conflict(
                "The session became active before enqueue.",
            ));
        }
        if settings["brain"]["approved"] == true
            && current["brain"]["context_id"] != settings["brain"]["context_id"]
        {
            return Err(HarnessError::conflict(
                "Prepared context changed; review it again.",
            ));
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM sessions WHERE status='queued'",
            [],
            |r| r.get(0),
        )?;
        if count >= 16 {
            return Err(HarnessError::conflict("The run queue is full."));
        }
        let (attached, mut guard) = crate::attachments::stage(&tx, &self.state, sid, uploads)?;
        for (key, value) in fields {
            let value = match value {
                Value::Null => rusqlite::types::Value::Null,
                Value::String(v) => rusqlite::types::Value::Text(v.clone()),
                Value::Bool(v) => rusqlite::types::Value::Integer(i64::from(*v)),
                Value::Number(v) if v.is_i64() => {
                    rusqlite::types::Value::Integer(v.as_i64().unwrap_or_default())
                }
                _ => rusqlite::types::Value::Text(value.to_string()),
            };
            tx.execute(
                &format!("UPDATE sessions SET {key}=?1 WHERE id=?2"),
                params![value, sid],
            )?;
        }
        tx.execute(
            "UPDATE sessions SET status='queued',updated_at=?1 WHERE id=?2",
            params![now(), sid],
        )?;
        tx.execute("INSERT INTO harness_generations(session_id,generation)VALUES(?1,?2)ON CONFLICT(session_id)DO UPDATE SET generation=excluded.generation",params![sid,hexid()])?;
        if let Some(prompt) = prompt {
            tx.execute(
                "INSERT INTO events(session_id,data)VALUES(?1,?2)",
                params![
                    sid,
                    json!({"kind":"user","text":prompt,"attachments":attached}).to_string()
                ],
            )?;
        }
        tx.commit()?;
        guard.commit();
        drop(db);
        self.get(sid)
    }
    pub fn merge_archive(&self, sid: &str) -> Result<Value> {
        crate::chat_merge::ChatMerge::new(self.clone()).archive(sid)
    }
    pub fn delete_merge(&self, sid: &str) -> Result<Value> {
        crate::chat_merge::ChatMerge::new(self.clone()).delete(sid)
    }
}
fn encode_option(value: &Value) -> Option<String> {
    if value.is_null() {
        None
    } else {
        Some(value.to_string())
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "A single SQLite transaction binds the frozen session row, first event, uploads and generation together."
)]
pub(crate) fn insert_session(
    tx: &Connection,
    options: &Value,
    project: &Value,
    prompt: &str,
    attachments: &[Value],
    sid: &str,
    at: &str,
    generation: &str,
) -> Result<()> {
    let provider = options["provider"]
        .as_str()
        .ok_or_else(|| HarnessError::invalid("Select a provider."))?;
    tx.execute("INSERT INTO sessions(id,title,project_id,project_path,provider,model,mode,workflow,project_context,status,native_session_id,created_at,updated_at,agents_enabled,agent_count,thinking_effort,workspace,branch,git_common_dir,fleet,fleet_action,brain,budgets,sdd,model_routing,clash)VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'queued',NULL,?10,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
   params![sid,prompt.chars().take(80).collect::<String>(),project["id"].as_str(),options["_workspace_path"].as_str().or_else(||project["path"].as_str()),provider,options["model"].as_str(),
   options["mode"].as_str().unwrap_or("plan"),options["workflow"].as_str().unwrap_or("native"),i64::from(options["project_context"].as_bool().unwrap_or(false)),
   at,i64::from(options["agents_enabled"].as_bool().unwrap_or(false)),options["agent_count"].as_i64().unwrap_or(3),options["thinking_effort"].as_str(),
   options["workspace"].as_str().unwrap_or("project"),options["branch"].as_str(),options["git_common_dir"].as_str(),encode_option(&options["fleet"]),if options["fleet"].is_null(){None}else{Some("start")},
   encode_option(&options["brain"]),encode_option(&options["budgets"]),encode_option(&options["sdd"]),encode_option(&options["model_routing"]),encode_option(&options["clash"])])?;
    tx.execute(
        "INSERT INTO events(session_id,data)VALUES(?1,?2)",
        params![
            sid,
            json!({"kind":"user","text":prompt,"attachments":attachments}).to_string()
        ],
    )?;
    tx.execute(
        "INSERT INTO harness_generations(session_id,generation)VALUES(?1,?2)",
        params![sid, generation],
    )?;
    Ok(())
}

pub fn agent_plan(session: &Value) -> Value {
    let fleet = &session["fleet"];
    let helpers = session["agent_count"].as_u64().unwrap_or(3);
    let count = if let Some(lenses) = fleet["lenses"].as_array() {
        lenses.len() as u64
    } else {
        1 + if session["agents_enabled"] == true {
            helpers
        } else {
            0
        }
    }
    .max(1);
    let concurrent = if fleet.is_null() {
        count
    } else {
        count.min(helpers)
    }
    .max(1);
    let tokens = session["budgets"]["tokens"].as_u64();
    let seconds = session["budgets"]["seconds"].as_u64();
    let worker = fleet["worker_timeout"].as_u64();
    let seconds = if fleet.is_null() {
        seconds
    } else {
        match (seconds, worker) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (_, b) => b,
        }
    };
    json!({"agents":count,"concurrent":concurrent,"waves":count.div_ceil(concurrent),"usd":session["budgets"]["usd"].as_f64().map(|v|(v/count as f64*1_000_000.).floor()/1_000_000.),"tokens":tokens.map(|n|n/count),"unallocated_tokens":tokens.map(|n|n%count),"seconds":seconds,"shared_seconds":session["budgets"]["seconds"],"scope":if fleet.is_null(){"main_and_helper_slots"}else{"reviewers"}})
}

pub(crate) fn get_session(db: &Connection, sid: &str) -> Result<Value> {
    let mut q = db.prepare(
        "SELECT s.*,m.summary FROM sessions s LEFT JOIN session_merges m ON m.session_id=s.id WHERE s.id=?1",
    )?;
    let names = session_columns(&q);
    let mut rows = q.query([sid])?;
    let row = rows
        .next()?
        .ok_or_else(|| HarnessError::missing("Session not found."))?;
    decode_session(row, &names)
}

fn session_columns(statement: &rusqlite::Statement<'_>) -> Vec<String> {
    // The final column belongs to the join, rather than the public session.
    statement
        .column_names()
        .iter()
        .take(statement.column_count() - 1)
        .map(|s| s.to_string())
        .collect()
}

fn decode_session(row: &rusqlite::Row<'_>, names: &[String]) -> Result<Value> {
    let mut result = Map::new();
    for (i, name) in names.iter().enumerate() {
        use rusqlite::types::ValueRef;
        let value = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => json!(n),
            ValueRef::Real(n) => json!(n),
            ValueRef::Text(s) => Value::String(
                std::str::from_utf8(s)
                    .map_err(|_| HarnessError::invalid("Invalid session text."))?
                    .into(),
            ),
            _ => return Err(HarnessError::invalid("Invalid session field.")),
        };
        result.insert(name.clone(), value);
    }
    for key in ["project_context", "agents_enabled"] {
        let value = result.get(key).and_then(Value::as_i64).unwrap_or(0) != 0;
        result.insert(key.into(), value.into());
    }
    for key in [
        "fleet",
        "fleet_result",
        "brain",
        "creator",
        "system_run",
        "system_discovery",
        "budgets",
        "budget_usage",
        "result_base",
        "sdd",
        "model_routing",
        "clash",
        "clash_result",
        "cost_totals",
    ] {
        if let Some(raw) = result.get(key).and_then(Value::as_str) {
            let parsed = crate::strict_json::parse(raw.as_bytes(), 16 * 1024 * 1024)?;
            result.insert(key.into(), parsed);
        }
    }
    result.remove("fleet_action");
    let summary: Option<String> = row.get(names.len())?;
    result.insert(
        "merge".into(),
        summary
            .map(|s| crate::strict_json::parse(s.as_bytes(), 2 * 1024 * 1024))
            .transpose()?
            .unwrap_or(Value::Null),
    );
    if result.get("budgets").is_none_or(Value::is_null) {
        let usd = result
            .get("fleet")
            .map(|v| v["budget_usd"].clone())
            .unwrap_or(Value::Null);
        result.insert(
            "budgets".into(),
            json!({"usd":usd,"tokens":null,"seconds":900}),
        );
    }
    let mut result = Value::Object(result);
    let plan = agent_plan(&result);
    result["agent_budget_plan"] = plan;
    Ok(result)
}

fn session_events(db: &Connection, sid: &str, after: i64) -> Result<Vec<Value>> {
    if after < 0 {
        return Err(HarnessError::invalid("Invalid event cursor."));
    }
    let mut q = db.prepare(
        "SELECT id,data FROM events WHERE session_id=?1 AND id>?2 ORDER BY id LIMIT 250",
    )?;
    let rows = q.query_map(params![sid, after], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, raw) = row?;
        let mut v = crate::strict_json::parse(raw.as_bytes(), 4 * 1024 * 1024)?;
        v["id"] = id.into();
        out.push(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, SessionStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&directory.path().join("state")).unwrap();
        (directory, store)
    }

    fn insert(db: &Connection, sid: &str, updated: &str) {
        db.execute(
            "INSERT INTO sessions(id,title,project_id,project_path,provider,mode,workflow,project_context,status,created_at,updated_at) VALUES(?1,'Example','project','/project','codex','plan','native',0,'completed','created',?2)",
            params![sid, updated],
        )
        .unwrap();
    }

    #[test]
    fn task_context_keeps_the_first_and_latest_requests_and_says_where_they_ran() {
        let (_directory, store) = fixture();
        {
            let db = store.connection().unwrap();
            // The reply's own time wins over a later session update.
            insert(&db, "source", "2026-11-10T08:00:00Z");
            insert(&db, "destination", "updated");
            db.execute(
                "UPDATE sessions SET branch='main',result_base=?1 WHERE id='source'",
                [json!({"head":"0123456789abcdef0123456789abcdef01234567","branch":"main","dirty":true,"is_git":true}).to_string()],
            )
            .unwrap();
        }
        for event in [
            json!({"kind":"user","text":"Earlier request"}),
            json!({"kind":"text","text":"Earlier reply"}),
            json!({"kind":"user","text":"Current request","attachments":[{"name":"attachment-only"}]}),
            json!({"kind":"text","text":"Progress only"}),
            json!({"kind":"text","at":"2026-11-09T14:32:02.123Z","text":format!("Latest reply\n\n```memory-draft\nprivate draft\n```\n{}", "界".repeat(4000))}),
            json!({"kind":"tool","text":"Tool output"}),
            json!({"kind":"text","text":"Recovery text","memory_recovery":true}),
            json!({"kind":"result","text":"Failed result","ok":false}),
            json!({"kind":"result","text":"","ok":true}),
        ] {
            store.event("source", &event).unwrap();
        }
        let response =
            crate::chat_merge::context_draft(&store, "source", "project", Some("destination"))
                .unwrap();
        let draft = response["draft"].as_str().unwrap();
        assert!(draft.starts_with("# Context from a task\n\nSource: Example (source)\n"));
        assert!(draft.contains(
            "\nRan: Codex in plan mode, in the project folder on branch main, started from commit 0123456789ab with uncommitted changes; replied 2026-11-09 14:32 UTC.\n"
        ));
        assert!(draft.contains(
            "\nHistory: 2 requests; this draft keeps the first and latest requests, the latest reply and 1 earlier turn, newest first.\n"
        ));
        assert!(draft.contains("\nCheck file and line references against the current checkout"));
        assert!(draft.contains("\n## First request\n\nEarlier request\n"));
        assert!(draft.contains("\n## Latest request\n\nCurrent request\n"));
        assert!(draft.contains("\n## Latest reply\n\nLatest reply\n"));
        // The reply takes what the draft has left, and the bound counts
        // characters: 4,000 of them in a multibyte script stay whole. The room
        // after it goes to the reply before it.
        let kept = draft.matches('界').count();
        assert!(
            kept == 4000
                && draft.ends_with("界\n\n## Reply to the first request\n\nEarlier reply\n")
                && !draft.contains("omitted"),
            "kept {kept}"
        );
        assert!(draft.chars().count() <= crate::chat_merge::TASK_CONTEXT_CHARS);
        assert!(!draft.contains('�'));
        for omitted in [
            "Request 1 of 2",
            "Progress only",
            "attachment-only",
            "private draft",
            "Tool output",
            "Recovery text",
            "Failed result",
        ] {
            assert!(!draft.contains(omitted), "leaked {omitted}");
        }
        assert_eq!(
            response["name"],
            "context-example-current-request-source.md"
        );
        assert_eq!(response["limit"], crate::chat_merge::TASK_CONTEXT_CHARS);
    }

    #[test]
    fn task_context_of_one_turn_reads_as_one_request_and_names_its_file_by_title() {
        let (_directory, store) = fixture();
        {
            let db = store.connection().unwrap();
            insert(&db, "single-0001", "updated");
            db.execute(
                "UPDATE sessions SET title=?1,provider='cursor',mode='edit',workspace='existing-worktree',branch='feature/x' WHERE id='single-0001'",
                ["Почему ИТОГ: off-by-one / v2?\n## Injected"],
            )
            .unwrap();
        }
        store
            .event("single-0001", &json!({"kind":"user","text":"Only request"}))
            .unwrap();
        // Neither the reply nor the session carries a readable time.
        store
            .event(
                "single-0001",
                &json!({"kind":"text","text":"Only reply","at":"later"}),
            )
            .unwrap();
        let response =
            crate::chat_merge::context_draft(&store, "single-0001", "project", None).unwrap();
        let draft = response["draft"].as_str().unwrap();
        assert!(
            draft.contains("Source: Почему ИТОГ: off-by-one / v2? ## Injected (single-0001)\n")
        );
        assert!(!draft.contains("\n## Injected"));
        assert!(draft.contains(
            "\nRan: Cursor Agent in edit mode, in an existing worktree on branch feature/x.\n"
        ));
        assert!(draft.contains("\n## Request\n\nOnly request\n\n## Reply\n\nOnly reply\n"));
        for absent in ["History:", "## First request", "started from", "; replied"] {
            assert!(!draft.contains(absent), "unexpected {absent}");
        }
        assert_eq!(
            response["name"],
            "context-почему-итог-off-by-one-v2-injected-single00.md"
        );
        let db = store.connection().unwrap();
        db.execute("UPDATE sessions SET title='?!' WHERE id='single-0001'", [])
            .unwrap();
        drop(db);
        let response =
            crate::chat_merge::context_draft(&store, "single-0001", "project", None).unwrap();
        assert_eq!(response["name"], "context-task-single00.md");
        let db = store.connection().unwrap();
        db.execute(
            "UPDATE sessions SET title='Find why invoice totals are off by a cent, again' WHERE id='single-0001'",
            [],
        )
        .unwrap();
        drop(db);
        let response =
            crate::chat_merge::context_draft(&store, "single-0001", "project", None).unwrap();
        assert_eq!(
            response["name"],
            "context-find-why-invoice-totals-are-off-by-a-single00.md"
        );
    }

    /// An agent sees nothing of an attachment but its name before it decides
    /// to open it. A chat that began with a status question and went on to the
    /// investigation was named after the status question, and the agent left
    /// the file unread or opened it last.
    #[test]
    fn task_context_of_a_longer_chat_is_named_after_its_first_and_its_longest_later_request() {
        let (_directory, store) = fixture();
        let name = |sid: &str, title: &str, requests: &[&str]| {
            let db = store.connection().unwrap();
            insert(&db, sid, "updated");
            db.execute(
                "UPDATE sessions SET title=?1 WHERE id=?2",
                params![title, sid],
            )
            .unwrap();
            drop(db);
            for request in requests {
                turn(
                    &store,
                    sid,
                    request,
                    &[json!({"kind":"text","text":"Reply"})],
                );
            }
            let response = crate::chat_merge::context_draft(&store, sid, "project", None).unwrap();
            response["name"].as_str().unwrap().to_owned()
        };
        // Follow-ups such as "go on" name nothing: the longest later request does.
        assert_eq!(
            name(
                "pivot-0001",
                "На какой ветке находится checkout?",
                &[
                    "На какой ветке находится checkout?",
                    "go on",
                    "TASK-DISPATCH. Опиши, как runner вызывает DispatchContext и что передаёт",
                    "use gh?",
                ],
            ),
            "context-на-какой-ветке-находится-checkout-task-dispatch-опиши-как-runner-вызывает-pivot000.md"
        );
        // The first request is never the later one, however long it is.
        assert_eq!(
            name(
                "first-0006",
                "Fix rounding",
                &[
                    "Fix rounding in the invoice totals, please",
                    "now the tests"
                ]
            ),
            "context-fix-rounding-now-the-tests-first000.md"
        );
        // Of equally long later requests the newest names the file.
        assert_eq!(
            name(
                "equal-0002",
                "Review",
                &["Review", "check tests", "fix docs 12"]
            ),
            "context-review-fix-docs-12-equal000.md"
        );
        // A later request that says the title again, or nothing a name can
        // carry, adds no second part.
        assert_eq!(
            name(
                "again-0003",
                "Fix rounding",
                &["Fix rounding", "fix ROUNDING!"]
            ),
            "context-fix-rounding-again000.md"
        );
        assert_eq!(
            name("marks-0004", "Fix rounding", &["Fix rounding", "?!", "…"]),
            "context-fix-rounding-marks000.md"
        );
        // A title without words leaves the later request alone in the name.
        assert_eq!(
            name("blank-0005", "?!", &["?!", "Why is the total off?"]),
            "context-why-is-the-total-off-blank000.md"
        );
    }

    /// An upload refuses a name over 200 bytes. Two parts in a script of three
    /// or four bytes a character share that room, each cut between its words.
    #[test]
    fn task_context_name_of_a_longer_chat_fits_an_attachment_name_in_any_script() {
        let (_directory, store) = fixture();
        for (sid, first, later) in [
            ("wide-0002", "ж", "я"),
            ("wide-0003", "界", "世"),
            ("wide-0004", "𝐀", "𝐁"),
        ] {
            let bytes = first.len();
            assert_eq!(later.len(), bytes);
            // Seven-letter words, more of them than any part holds.
            let text = |letter: &str, words: usize| vec![letter.repeat(7); words].join(" ");
            let db = store.connection().unwrap();
            insert(&db, sid, "updated");
            db.execute(
                "UPDATE sessions SET title=?1 WHERE id=?2",
                params![text(first, 9), sid],
            )
            .unwrap();
            drop(db);
            let reply = [json!({"kind":"text","text":"Reply"})];
            turn(&store, sid, &text(first, 9), &reply);
            turn(&store, sid, &text(later, 12), &reply);
            let response = crate::chat_merge::context_draft(&store, sid, "project", None).unwrap();
            let name = response["name"].as_str().unwrap();
            assert!(name.len() <= crate::attachments::NAME_BYTES, "{name}");
            assert!(
                crate::attachments::validate(&json!([{"name":name,"data":""}])).is_ok(),
                "{name}"
            );
            // Whole words only: as many as 40 characters and half the room hold.
            let words = ((40 + 1) / 8).min((89 + 1) / (7 * bytes + 1));
            let part = |letter: &str| vec![letter.repeat(7); words].join("-");
            assert_eq!(
                name,
                format!(
                    "context-{}-{}-{}.md",
                    part(first),
                    part(later),
                    sid.replace('-', "")
                ),
                "{bytes} bytes a character"
            );
        }
    }

    #[test]
    fn task_context_stays_within_its_bound_at_every_limit() {
        let (_directory, store) = fixture();
        {
            let db = store.connection().unwrap();
            insert(&db, "wide", "2026-11-09T14:32:02Z");
            db.execute(
                "UPDATE sessions SET title=?1,provider='cursor',mode='edit',workspace='existing-worktree',branch=?2,result_base=?3 WHERE id='wide'",
                params![
                    "ж".repeat(400),
                    "b".repeat(256),
                    json!({"head":"f".repeat(40),"branch":"main","dirty":true,"is_git":true}).to_string()
                ],
            )
            .unwrap();
        }
        for (kind, text) in [
            ("user", "ю".repeat(5000)),
            ("text", "a".repeat(100)),
            ("user", "я".repeat(5000)),
            ("text", "界".repeat(20000)),
        ] {
            store
                .event("wide", &json!({"kind":kind,"text":text}))
                .unwrap();
        }
        let response = crate::chat_merge::context_draft(&store, "wide", "project", None).unwrap();
        let draft = response["draft"].as_str().unwrap();
        assert!(draft.contains("## First request") && draft.contains("## Latest reply"));
        // Both requests and the reply are cut, each around its own marker, and
        // with every other part at its widest the reply still gets its least.
        assert_eq!(draft.matches(" characters omitted …]").count(), 3);
        let kept = draft.matches('界').count();
        assert!((2900..=3000).contains(&kept), "kept {kept}");
        for request in ['ю', 'я'] {
            let kept = draft.matches(request).count();
            assert!((900..=1000).contains(&kept), "{request} kept {kept}");
        }
        // A draft whose latest reply was cut has no room for the turn before.
        assert!(!draft.contains("## Reply to the first request") && !draft.contains("aaaa"));
        assert!(draft.contains("\nHistory: 2 requests; this draft keeps the first and latest requests and the latest reply.\n"));
        assert!(
            draft.chars().count() <= crate::chat_merge::TASK_CONTEXT_CHARS,
            "{} characters",
            draft.chars().count()
        );
        let name = response["name"].as_str().unwrap();
        assert!(name.len() <= 200 && name.ends_with("-wide.md"), "{name}");
    }

    /// A ticket ends with its constraints and a report with what it left out
    /// or did not verify. A cut that kept only the beginning dropped both.
    #[test]
    fn task_context_cuts_a_long_request_and_reply_in_the_middle_on_whole_lines() {
        let (_directory, store) = fixture();
        insert(
            &store.connection().unwrap(),
            "report",
            "2026-11-10T08:00:00Z",
        );
        let ticket = format!(
            "# TICKET-1 Role-gate the contact directory\n\n{}\nDo not commit or push. REQUEST-END",
            "Background line about the directory and who may read it.\n".repeat(120)
        );
        let reply = format!(
            "## RESULT\n\nThe directory is role-gated. REPLY-START\n\n{}\n## Deliberately left out\n\n- The search index has the same leak. REPLY-END",
            (1..=160)
                .map(|n| format!("- src/File{n}.php: changed and covered by a test.\n"))
                .collect::<String>()
        );
        for event in [
            json!({"kind":"user","text":ticket}),
            json!({"kind":"text","text":reply}),
        ] {
            store.event("report", &event).unwrap();
        }
        let response = crate::chat_merge::context_draft(&store, "report", "project", None).unwrap();
        let draft = response["draft"].as_str().unwrap();
        for kept in [
            "# TICKET-1",
            "REQUEST-END",
            "## RESULT",
            "REPLY-START",
            "## Deliberately left out",
            "REPLY-END",
        ] {
            assert!(draft.contains(kept), "lost {kept}");
        }
        let (request, answer) = draft.split_once("\n\n## Reply\n\n").unwrap();
        let request = request.split_once("\n\n## Request\n\n").unwrap().1;
        let answer = answer.strip_suffix('\n').unwrap();
        for (kept, original) in [(request, ticket.as_str()), (answer, reply.as_str())] {
            // The marker counts what it stands for, and both kept parts are
            // whole lines of the original.
            let (head, rest) = kept.split_once("\n\n[… ").unwrap();
            let (omitted, tail) = rest.split_once(" characters omitted …]\n\n").unwrap();
            let omitted: usize = omitted.parse().unwrap();
            assert_eq!(
                head.chars().count() + tail.chars().count() + omitted,
                original.chars().count()
            );
            assert!(original.starts_with(head) && original[head.len()..].starts_with('\n'));
            assert!(
                original.ends_with(tail) && original[..original.len() - tail.len()].ends_with('\n')
            );
            // The beginning gets about twice the room of the end.
            assert!(head.len() > tail.len() && head.len() < 3 * tail.len());
        }
        assert!(request.chars().count() <= 1000);
        // The reply got more than the least it is given, and a draft whose
        // reply was cut leaves the reader room for a note.
        assert!(answer.chars().count() > 3000, "{}", answer.chars().count());
        let chars = draft.chars().count();
        assert!(
            chars
                <= crate::chat_merge::TASK_CONTEXT_CHARS
                    - crate::chat_merge::TASK_CONTEXT_NOTE_CHARS,
            "{chars} characters"
        );
    }

    fn turn(store: &SessionStore, sid: &str, request: &str, replies: &[Value]) {
        store
            .event(sid, &json!({"kind":"user","text":request}))
            .unwrap();
        for reply in replies {
            store.event(sid, reply).unwrap();
        }
    }

    /// The latest reply of a long chat is often a short follow-up, and what
    /// the chat established stands in the replies before it.
    #[test]
    fn task_context_adds_earlier_turns_newest_first_while_it_has_room() {
        let (_directory, store) = fixture();
        insert(&store.connection().unwrap(), "long", "updated");
        let ticket = format!(
            "Third ask: {}Keep the public API. THIRD-END",
            "explain the failing criteria one by one. ".repeat(20)
        );
        turn(
            &store,
            "long",
            "First task",
            &[json!({"kind":"text","text":"Reply one: nothing to change yet."})],
        );
        // Asked and never answered: a tool ran and the run stopped.
        turn(
            &store,
            "long",
            "Second ask",
            &[json!({"kind":"tool","text":"Tool output two"})],
        );
        turn(
            &store,
            "long",
            &ticket,
            &[
                json!({"kind":"result","ok":true,"text":"Reply three: two criteria fail.\n\n```memory-draft\nprivate three\n```"}),
            ],
        );
        turn(
            &store,
            "long",
            "Fourth ask",
            &[
                json!({"kind":"text","text":"Progress four"}),
                json!({"kind":"text","text":"Reply four: all criteria met, release pending."}),
                json!({"kind":"text","text":"Recovery four","memory_recovery":true}),
            ],
        );
        turn(
            &store,
            "long",
            "Latest ask",
            &[json!({"kind":"text","text":"Latest answer: pushed."})],
        );
        let response = crate::chat_merge::context_draft(&store, "long", "project", None).unwrap();
        let draft = response["draft"].as_str().unwrap();
        assert!(draft.contains(
            "\nHistory: 5 requests; this draft keeps the first and latest requests, the latest reply and 3 earlier turns, newest first.\n"
        ));
        // Newest first under the latest reply, each reply under its request;
        // the first request is not repeated above its reply.
        let sections: Vec<&str> = draft
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect();
        assert_eq!(
            sections,
            [
                "## First request",
                "## Latest request",
                "## Latest reply",
                "## Request 4 of 5",
                "## Reply to request 4",
                "## Request 3 of 5",
                "## Reply to request 3",
                "## Reply to the first request",
            ]
        );
        assert!(draft.contains(
            "\n## Latest reply\n\nLatest answer: pushed.\n\n## Request 4 of 5\n\nFourth ask\n\n## Reply to request 4\n\nReply four: all criteria met, release pending.\n\n## Request 3 of 5\n\nThird ask: "
        ));
        assert!(draft.ends_with(
            "\n\n## Reply to request 3\n\nReply three: two criteria fail.\n\n## Reply to the first request\n\nReply one: nothing to change yet.\n"
        ));
        // An earlier request keeps its beginning and its end in a shorter cut.
        let third = draft
            .split_once("\n## Request 3 of 5\n\n")
            .unwrap()
            .1
            .split_once("\n\n## Reply to request 3")
            .unwrap()
            .0;
        assert!(third.chars().count() <= 400 && third.ends_with("THIRD-END"));
        assert_eq!(third.matches(" characters omitted …]").count(), 1);
        for omitted in [
            "Second ask",
            "Tool output two",
            "private three",
            "Progress four",
            "Recovery four",
        ] {
            assert!(!draft.contains(omitted), "leaked {omitted}");
        }
    }

    /// The turn before a short follow-up may hold the whole report: it is cut
    /// like a latest reply, into the room the draft has left under its note.
    #[test]
    fn task_context_cuts_an_earlier_reply_into_the_room_left_under_the_note() {
        let (_directory, store) = fixture();
        insert(&store.connection().unwrap(), "report", "updated");
        let report = format!(
            "## RESULT\n\nDone. REPORT-START\n\n{}\n## Left out\n\n- The export. REPORT-END",
            "- A line of the report about one changed file.\n".repeat(220)
        );
        turn(
            &store,
            "report",
            "Old task",
            &[json!({"kind":"text","text":format!("OLD-MARK {}", "o".repeat(1500))})],
        );
        turn(
            &store,
            "report",
            "Implement it",
            &[json!({"kind":"text","text":report})],
        );
        turn(
            &store,
            "report",
            "commit",
            &[json!({"kind":"text","text":"Committed 86b845ec1."})],
        );
        let response = crate::chat_merge::context_draft(&store, "report", "project", None).unwrap();
        let draft = response["draft"].as_str().unwrap();
        assert!(draft.contains("and 1 earlier turn, newest first.\n"));
        let earlier = draft
            .split_once("\n## Request 2 of 3\n\nImplement it\n\n## Reply to request 2\n\n")
            .unwrap()
            .1;
        assert!(earlier.starts_with("## RESULT\n\nDone. REPORT-START\n"));
        assert!(earlier.ends_with("\n## Left out\n\n- The export. REPORT-END\n"));
        assert_eq!(earlier.matches(" characters omitted …]").count(), 1);
        // The cut reply used the room up to the reader's note: what is left
        // would cut the turn before it to a few lines, so that turn stays out.
        let chars = draft.chars().count();
        let fill =
            crate::chat_merge::TASK_CONTEXT_CHARS - crate::chat_merge::TASK_CONTEXT_NOTE_CHARS;
        assert!((fill - 200..=fill).contains(&chars), "{chars} characters");
        assert!(!draft.contains("OLD-MARK") && !draft.contains("Reply to the first request"));
    }

    /// With little room left a reply that fits is still added, and one that
    /// would be cut to a few lines is not.
    #[test]
    fn task_context_adds_no_earlier_reply_that_little_room_would_cut() {
        for (earlier, added) in [
            ("Short earlier answer.".to_owned(), true),
            ("e".repeat(2000), false),
        ] {
            let (_directory, store) = fixture();
            insert(&store.connection().unwrap(), "tight", "updated");
            turn(
                &store,
                "tight",
                "First task",
                &[json!({"kind":"text","text":earlier})],
            );
            // The latest reply fits whole and leaves under 600 characters
            // before the room kept for a note.
            turn(
                &store,
                "tight",
                "Latest ask",
                &[json!({"kind":"text","text":"l".repeat(4800)})],
            );
            let response =
                crate::chat_merge::context_draft(&store, "tight", "project", None).unwrap();
            let draft = response["draft"].as_str().unwrap();
            assert!(draft.matches('l').count() >= 4800);
            assert_eq!(
                draft.contains("\n## Reply to the first request\n\n"),
                added,
                "{} characters",
                draft.chars().count()
            );
            assert_eq!(draft.contains("1 earlier turn"), added);
            assert!(
                draft.chars().count()
                    <= crate::chat_merge::TASK_CONTEXT_CHARS
                        - crate::chat_merge::TASK_CONTEXT_NOTE_CHARS
            );
        }
    }

    /// A chat of very many turns is read a bounded number of turns back.
    #[test]
    fn task_context_reads_a_bounded_number_of_earlier_turns() {
        let (_directory, store) = fixture();
        insert(&store.connection().unwrap(), "many", "updated");
        for n in 1..=40 {
            turn(
                &store,
                "many",
                &format!("Ask {n}"),
                &[json!({"kind":"text","text":format!("Answer {n}.")})],
            );
        }
        let response = crate::chat_merge::context_draft(&store, "many", "project", None).unwrap();
        let draft = response["draft"].as_str().unwrap();
        assert!(draft.contains(
            "\nHistory: 40 requests; this draft keeps the first and latest requests, the latest reply and 32 earlier turns, newest first.\n"
        ));
        assert!(draft.contains("\n## Reply to request 39\n\nAnswer 39.\n"));
        assert!(
            draft
                .ends_with("\n## Request 8 of 40\n\nAsk 8\n\n## Reply to request 8\n\nAnswer 8.\n")
        );
        assert!(!draft.contains("Answer 7."));
        assert!(
            draft.chars().count()
                <= crate::chat_merge::TASK_CONTEXT_CHARS
                    - crate::chat_merge::TASK_CONTEXT_NOTE_CHARS
        );
    }

    /// A reply that fits what the draft has left stays whole even when it is
    /// longer than the least a reply is given.
    #[test]
    fn task_context_keeps_a_reply_whole_while_the_draft_has_room() {
        let (_directory, store) = fixture();
        insert(&store.connection().unwrap(), "fits", "updated");
        let reply = format!(
            "## RESULT\n\n{}\n## Deliberately left out\n\n- Nothing. REPLY-END",
            "A line of the report that says what was done.\n".repeat(100)
        );
        assert!((4500..5000).contains(&reply.chars().count()));
        for event in [
            json!({"kind":"user","text":"Only request"}),
            json!({"kind":"result","ok":true,"text":reply}),
        ] {
            store.event("fits", &event).unwrap();
        }
        let response = crate::chat_merge::context_draft(&store, "fits", "project", None).unwrap();
        let draft = response["draft"].as_str().unwrap();
        assert!(draft.ends_with(&format!("\n\n## Reply\n\n{reply}\n")));
        assert!(!draft.contains("omitted"));
        assert!(draft.chars().count() <= crate::chat_merge::TASK_CONTEXT_CHARS);
    }

    /// The end of a message longer than the store reads is unknown, so such a
    /// message keeps its beginning and counts the rest, read or not.
    #[test]
    fn task_context_of_a_message_too_long_to_read_keeps_its_beginning() {
        let (_directory, store) = fixture();
        insert(&store.connection().unwrap(), "huge", "updated");
        let ticket = format!("REQUEST-START {} REQUEST-END", "y".repeat(140_000));
        let reply = format!("REPLY-START {} REPLY-END", "x".repeat(140_000));
        for event in [
            json!({"kind":"user","text":ticket}),
            json!({"kind":"text","text":reply}),
        ] {
            store.event("huge", &event).unwrap();
        }
        let response = crate::chat_merge::context_draft(&store, "huge", "project", None).unwrap();
        let draft = response["draft"].as_str().unwrap();
        assert!(draft.contains("REQUEST-START") && draft.contains("REPLY-START"));
        assert!(!draft.contains("REQUEST-END") && !draft.contains("REPLY-END"));
        let (request, answer) = draft.split_once("\n\n## Reply\n\n").unwrap();
        let request = request.split_once("\n\n## Request\n\n").unwrap().1;
        for (kept, original) in [(request, &ticket), (answer.trim_end(), &reply)] {
            let (head, omitted) = kept.split_once("\n\n[… ").unwrap();
            let omitted: usize = omitted
                .strip_suffix(" characters omitted …]")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(head.chars().count() + omitted, original.chars().count());
        }
        assert!(draft.chars().count() <= crate::chat_merge::TASK_CONTEXT_CHARS);
    }

    #[test]
    fn task_context_rejects_self_cross_project_active_and_missing_replies() {
        let (_directory, store) = fixture();
        {
            let db = store.connection().unwrap();
            for id in ["source", "active", "other", "special"] {
                insert(&db, id, "updated");
            }
            db.execute_batch(
                "UPDATE sessions SET status='running' WHERE id='active';
                              UPDATE sessions SET project_id='other-project' WHERE id='other';
                              UPDATE sessions SET workflow='fleet-review' WHERE id='special';",
            )
            .unwrap();
        }
        for (source, project, destination) in [
            ("source", "project", Some("source")),
            ("source", "project", Some("active")),
            ("source", "project", Some("other")),
            ("other", "project", None),
            ("active", "project", None),
            ("special", "project", None),
        ] {
            assert!(
                crate::chat_merge::context_draft(&store, source, project, destination).is_err()
            );
        }
        store
            .event("source", &json!({"kind":"user","text":"No answer yet"}))
            .unwrap();
        let err = crate::chat_merge::context_draft(&store, "source", "project", None).unwrap_err();
        assert!(err.message.contains("no completed reply"));
    }

    fn merge(db: &Connection, sid: &str, summary: &str) {
        db.execute(
            "INSERT INTO session_merges(session_id,request_id,request_hash,bundle,summary,context) VALUES(?1,?1,'hash','{}',?2,'context')",
            params![sid, summary],
        )
        .unwrap();
    }

    fn error<T>(result: Result<T>) -> (u16, &'static str, String) {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => (error.status, error.code, error.message),
        }
    }

    #[test]
    fn summaries_preserve_timestamp_and_rowid_order_and_cap() {
        let (_directory, store) = fixture();
        {
            let db = store.connection().unwrap();
            for i in 0..205 {
                insert(&db, &format!("session-{i}"), &format!("updated-{}", i % 3));
            }
            merge(&db, "session-204", r#"{"sources":["earlier"]}"#);
        }
        let summaries = store.summaries().unwrap();
        let mut expected = (0..205).collect::<Vec<_>>();
        expected.sort_by_key(|i| std::cmp::Reverse((i % 3, *i)));
        let expected = expected
            .into_iter()
            .take(200)
            .map(|i| format!("session-{i}"))
            .collect::<Vec<_>>();
        assert_eq!(summaries.len(), 200);
        for (summary, sid) in summaries.iter().zip(&expected) {
            assert_eq!(summary["id"], sid.as_str());
            assert_eq!(*summary, store.get(sid).unwrap());
            assert!(!summary.as_object().unwrap().contains_key("summary"));
        }
        assert_eq!(
            summaries.iter().find(|s| s["id"] == "session-204").unwrap()["merge"],
            json!({"sources":["earlier"]})
        );
    }

    #[test]
    fn joined_sessions_keep_normalization_defaults_and_null_merge() {
        let (_directory, store) = fixture();
        {
            let db = store.connection().unwrap();
            insert(&db, "session", "updated");
            db.execute(
                "UPDATE sessions SET project_context=2,agents_enabled=-1,fleet=?1,fleet_action='private',brain=?2 WHERE id='session'",
                params![r#"{"budget_usd":12}"#, r#"{"task":"TASK-1"}"#],
            )
            .unwrap();
        }
        let session = store.get("session").unwrap();
        assert_eq!(session["project_context"], true);
        assert_eq!(session["agents_enabled"], true);
        assert_eq!(session["brain"], json!({"task":"TASK-1"}));
        assert_eq!(
            session["budgets"],
            json!({"usd":12,"tokens":null,"seconds":900})
        );
        assert_eq!(session["agent_budget_plan"]["agents"], 4);
        assert_eq!(session["agent_budget_plan"]["concurrent"], 3);
        assert_eq!(session["agent_budget_plan"]["usd"], 3.0);
        assert_eq!(session["merge"], Value::Null);
        assert!(!session.as_object().unwrap().contains_key("fleet_action"));
        assert!(!session.as_object().unwrap().contains_key("summary"));
        assert_eq!(store.summaries().unwrap(), vec![session]);
    }

    #[test]
    fn summaries_validate_selected_json_without_reading_older_corruption() {
        let (_directory, store) = fixture();
        {
            let db = store.connection().unwrap();
            for i in 0..205 {
                insert(&db, &format!("session-{i}"), "updated");
            }
            db.execute(
                "UPDATE sessions SET brain=?1 WHERE id='session-0'",
                [r#"{"task":1,"task":2}"#],
            )
            .unwrap();
            merge(&db, "session-1", r#"{"sources":1,"sources":2}"#);
        }
        assert_eq!(store.summaries().unwrap().len(), 200);
        store
            .connection()
            .unwrap()
            .execute(
                "UPDATE sessions SET updated_at='z-newest' WHERE id='session-0'",
                [],
            )
            .unwrap();
        assert_eq!(error(store.summaries()), error(store.get("session-0")));
        {
            let db = store.connection().unwrap();
            db.execute("UPDATE sessions SET brain=NULL WHERE id='session-0'", [])
                .unwrap();
            db.execute(
                "UPDATE sessions SET updated_at='z-newest' WHERE id='session-1'",
                [],
            )
            .unwrap();
        }
        assert_eq!(error(store.summaries()), error(store.get("session-1")));
    }

    #[test]
    fn combined_poll_preserves_event_order_cursor_limit_and_session() {
        let (_directory, store) = fixture();
        {
            let db = store.connection().unwrap();
            insert(&db, "session", "updated");
            insert(&db, "other", "updated");
        }
        store.event("other", &json!({"kind":"status"})).unwrap();
        for sequence in 0..251 {
            store
                .event(
                    "session",
                    &json!({"kind":"status","sequence":sequence,"id":"untrusted"}),
                )
                .unwrap();
        }
        let (session, events) = store.session_events("session", 0).unwrap();
        assert_eq!(session, store.get("session").unwrap());
        assert_eq!(events, store.events("session", 0).unwrap());
        assert_eq!(events.len(), 250);
        assert_eq!(events.first().unwrap()["id"], 2);
        assert_eq!(events.first().unwrap()["sequence"], 0);
        assert_eq!(events.last().unwrap()["id"], 251);
        assert_eq!(events.last().unwrap()["sequence"], 249);
        let (_, tail) = store.session_events("session", 251).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0]["id"], 252);
        assert_eq!(tail[0]["sequence"], 250);
        assert!(store.session_events("session", 252).unwrap().1.is_empty());
    }

    #[test]
    fn poll_error_order_and_strict_session_and_event_json_are_preserved() {
        let (_directory, store) = fixture();
        assert_eq!(
            error(store.events("missing", -1)).2,
            "Invalid event cursor."
        );
        assert_eq!(
            error(store.session_events("missing", -1)).2,
            "Session not found."
        );
        assert_eq!(error(store.events("missing", 0)).0, 404);
        assert_eq!(error(store.session_events("missing", 0)).0, 404);
        {
            let db = store.connection().unwrap();
            insert(&db, "session", "updated");
            db.execute(
                "UPDATE sessions SET brain=?1 WHERE id='session'",
                [r#"{"a":1,"a":2}"#],
            )
            .unwrap();
        }
        let invalid_session = error(store.get("session"));
        assert_eq!(error(store.events("session", 0)), invalid_session);
        assert_eq!(error(store.session_events("session", -1)), invalid_session);
        assert_eq!(
            error(store.events("session", -1)).2,
            "Invalid event cursor."
        );
        {
            let db = store.connection().unwrap();
            db.execute("UPDATE sessions SET brain=NULL WHERE id='session'", [])
                .unwrap();
            db.execute(
                "INSERT INTO events(session_id,data) VALUES('session',?1)",
                [r#"{"kind":"status","kind":"other"}"#],
            )
            .unwrap();
        }
        assert_eq!(
            error(store.session_events("session", -1)).2,
            "Invalid event cursor."
        );
        let invalid_event = error(store.events("session", 0));
        assert_eq!(invalid_event.0, 400);
        assert_eq!(error(store.session_events("session", 0)), invalid_event);
        assert!(store.session_events("session", 1).unwrap().1.is_empty());
    }
}
