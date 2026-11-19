//! Local blackboard backed by standalone orch-core dispatch mailboxes and SessionStore state.
use crate::{
    HarnessError, Result, fleet::Blackboard, sessions::BlackboardFactory, store::SessionStore,
};
use orch_core::{
    capsule::validate_capsule_text,
    dispatch::LocalDispatchChannel,
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

pub struct LocalBlackboards {
    pub store: SessionStore,
}

pub struct LocalBlackboard {
    pub state_root: PathBuf,
    pub task_store: PathBuf,
}

impl BlackboardFactory for LocalBlackboards {
    fn for_session(&self, session: &Value) -> Result<Arc<dyn Blackboard>> {
        let project = session["project_path"]
            .as_str()
            .ok_or_else(|| HarnessError::invalid("Session workspace is missing."))?;
        let state_root = self.store.state.join("blackboard");
        orch_core::fs::ensure_directory(&state_root, true)?;
        let _ = project;
        Ok(Arc::new(LocalBlackboard {
            task_store: state_root.join("tasks.sqlite3"),
            state_root,
        }))
    }
}

impl LocalBlackboard {
    fn channel(&self) -> Result<LocalDispatchChannel> {
        Ok(LocalDispatchChannel::new(&self.state_root)?)
    }

    fn conn(&self) -> Result<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(&self.task_store)?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                task_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                progress TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )?;
        Ok(conn)
    }
}

impl Blackboard for LocalBlackboard {
    fn available(&self) -> bool {
        true
    }

    fn ensure_task(&self, task: &str, title: &str) -> Result<()> {
        let conn = self.conn()?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE task_id=?1)",
            [task],
            |r| r.get(0),
        )?;
        if !exists {
            conn.execute(
                "INSERT INTO tasks(task_id, title, progress, updated_at) VALUES(?1, ?2, '', ?3)",
                rusqlite::params![task, title, orch_core::domain::utc_now()],
            )?;
        }
        Ok(())
    }

    fn dispatch_spawn(&self, task: &str, lens: &str, capsule: &str) -> Result<()> {
        let problems = validate_capsule_text(capsule)?;
        if !problems.is_empty() {
            return Err(HarnessError::invalid(format!(
                "Delegation capsule is under-specified: {}",
                problems.join("; ")
            )));
        }
        self.channel()?
            .dispatch(task, lens, "spawn", None, Some(capsule))?;
        Ok(())
    }

    fn dispatch_complete(&self, task: &str, lens: &str, message: &str) -> Result<()> {
        let note = message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(160)
            .collect::<String>();
        self.channel()?
            .dispatch(task, lens, "complete", Some(&note), None)?;
        Ok(())
    }

    fn progress(&self, task: &str, message: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE tasks SET progress=?1, updated_at=?2 WHERE task_id=?3",
            rusqlite::params![message, orch_core::domain::utc_now(), task],
        )?;
        let note = json!({ "task": task, "progress": message });
        let _ = self.channel()?.dispatch(
            task,
            "blackboard",
            "complete",
            Some(&note.to_string().chars().take(160).collect::<String>()),
            None,
        );
        Ok(())
    }
}
