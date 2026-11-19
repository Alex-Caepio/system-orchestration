//! Working memory domain backed by SQLite run_notes and RunWorkingMemory snapshots.
use crate::{
    HarnessError, Result,
    services::{Domain, Payload, Reply},
    store::{self, SessionStore},
};
use orch_system::working_memory::notes::WorkingMemoryStore;
use serde_json::{Value, json};

pub struct WorkingMemoryDomain {
    pub store: SessionStore,
}

impl WorkingMemoryDomain {
    pub fn new(store: SessionStore) -> Self {
        Self { store }
    }

    fn db(&self) -> Result<WorkingMemoryStore> {
        WorkingMemoryStore::open(&self.store.state.join("working-memory"))
            .map_err(|e| HarnessError::unavailable(format!("Working memory unavailable: {e}")))
    }

    pub fn read(&self, scope_id: &str, include_resolved: bool) -> Result<Value> {
        self.db()?
            .read_notes(scope_id, include_resolved)
            .map_err(|e| HarnessError::invalid(format!("Failed to read working memory: {e}")))
    }

    pub fn write(&self, data: &Value) -> Result<Value> {
        let scope_id = data["scope_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HarnessError::invalid("Missing scope_id."))?;
        let key = data["key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HarnessError::invalid("Missing key."))?;
        let kind = data["kind"].as_str().unwrap_or("fact");
        let status = data["status"].as_str().unwrap_or("open");
        let text = data["text"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| HarnessError::invalid("Missing text."))?;
        let sources = data["sources"].as_array().cloned().unwrap_or_default();
        let ttl_seconds = data["ttl_seconds"].as_u64().unwrap_or(3600);
        self.db()?
            .write_note(scope_id, key, kind, status, text, &sources, ttl_seconds)
            .map_err(|e| HarnessError::invalid(format!("Working memory write rejected: {e}")))
    }
}

impl Domain for WorkingMemoryDomain {
    fn execute<'a>(&'a self, operation: &'a str, request: Value) -> Reply<'a> {
        Box::pin(async move {
            let params = &request["params"];
            let query = &request["query"];
            let body = &request["body"];
            let value = match operation {
                "working_memory.read" => {
                    let scope = params["session_id"]
                        .as_str()
                        .or_else(|| query["scope_id"].as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| HarnessError::invalid("Missing scope_id."))?;
                    let include_resolved = query["include_resolved"].as_str() == Some("true");
                    self.read(scope, include_resolved)?
                }
                "working_memory.write" => {
                    let mut payload = body.clone();
                    if let Some(sid) = params["session_id"].as_str().filter(|s| !s.is_empty()) {
                        payload["scope_id"] = json!(sid);
                    }
                    self.write(&payload)?
                }
                "working_memory.session_clear" => {
                    if !store::exact(body, &[], &[]) {
                        return Err(HarnessError::invalid("Expected empty object."));
                    }
                    json!({ "ok": true })
                }
                _ => return Err(HarnessError::missing("Unknown working memory operation.")),
            };
            Ok(Payload::Json(value))
        })
    }
}
