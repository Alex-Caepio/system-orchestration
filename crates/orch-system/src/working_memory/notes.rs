//! Standalone SQLite working-memory note store for cross-worker & cross-session state.
use super::{MAX_TTL_MS, SNAPSHOT_BYTES, now_ms};
use crate::{common, native};
use anyhow::{Context, Result, bail};
use orch_core::publication::PublicationFiles;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::path::Path;

pub struct WorkingMemoryStore {
    connection: Connection,
}

impl WorkingMemoryStore {
    pub fn open(directory: &Path) -> Result<Self> {
        native::private_directory(directory)?;
        let mut custody = PublicationFiles::new(directory)?;
        let name = "working-memory.sqlite3";
        #[cfg(unix)]
        let before = custody.metadata_image_identity(name)?;
        #[cfg(not(unix))]
        let before = custody.image_identity(name)?;
        if before.is_some_and(|image| image.3 != 1) {
            bail!("working_memory_cache_file")
        }
        let connection = Connection::open_with_flags(
            directory.join(name),
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        #[cfg(unix)]
        let opened = custody
            .metadata_image_identity(name)?
            .context("working_memory_cache_file")?;
        #[cfg(not(unix))]
        let opened = custody
            .image_identity(name)?
            .context("working_memory_cache_file")?;
        if opened.3 != 1 || before.is_some_and(|before| before != opened) {
            bail!("working_memory_cache_file_changed")
        }
        #[cfg(unix)]
        custody.secure_private_file_metadata(name, opened)?;
        #[cfg(not(unix))]
        custody.secure_private_file(name)?;
        custody.check()?;
        connection.busy_timeout(std::time::Duration::from_millis(100))?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_notes (
                scope_id TEXT NOT NULL,
                key TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                text TEXT NOT NULL,
                sources TEXT NOT NULL,
                updated_ms INTEGER NOT NULL,
                expires_ms INTEGER NOT NULL,
                PRIMARY KEY (scope_id, key)
            );",
        )?;
        Ok(Self { connection })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn write_note(
        &self,
        scope_id: &str,
        key: &str,
        kind: &str,
        status: &str,
        text: &str,
        sources: &[Value],
        ttl_seconds: u64,
    ) -> Result<Value> {
        if scope_id.trim().is_empty() || scope_id.len() > 128 {
            bail!("invalid scope_id");
        }
        if key.is_empty()
            || key.len() > 80
            || !key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
        {
            bail!("invalid working-memory key");
        }
        if !["progress", "hypothesis", "open_question", "decision", "fact"].contains(&kind) {
            bail!("invalid working-memory kind");
        }
        if !["open", "resolved"].contains(&status) {
            bail!("invalid working-memory status");
        }
        if text.trim().is_empty() || text.chars().count() > 1000 || text.len() > SNAPSHOT_BYTES {
            bail!("invalid working-memory text");
        }
        if common::sensitive(text)
            || orch_core::domain::secret_label(text)?.is_some()
            || orch_core::domain::secret_label(key)?.is_some()
        {
            bail!("sensitive text refused in working memory");
        }
        let sources_json = serde_json::to_string(sources)?;
        if orch_core::domain::secret_label(&sources_json)?.is_some() {
            bail!("sensitive citation refused in working memory");
        }
        let now = now_ms();
        let ttl_ms = ttl_seconds
            .saturating_mul(1000)
            .clamp(1000, MAX_TTL_MS);
        let expires = now.saturating_add(ttl_ms);
        self.connection
            .execute("DELETE FROM run_notes WHERE expires_ms <= ?1", [now])?;
        self.connection.execute(
            "INSERT OR REPLACE INTO run_notes(scope_id, key, kind, status, text, sources, updated_ms, expires_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![scope_id, key, kind, status, text.trim(), sources_json, now, expires],
        )?;
        Ok(json!({
            "scope_id": scope_id,
            "key": key,
            "kind": kind,
            "status": status,
            "text": text.trim(),
            "sources": sources,
            "updated_ms": now,
            "expires_ms": expires,
        }))
    }

    pub fn read_notes(&self, scope_id: &str, include_resolved: bool) -> Result<Value> {
        let now = now_ms();
        self.connection
            .execute("DELETE FROM run_notes WHERE expires_ms <= ?1", [now])?;
        let mut stmt = self.connection.prepare(
            "SELECT key, kind, status, text, sources, updated_ms, expires_ms
             FROM run_notes
             WHERE scope_id = ?1 AND expires_ms > ?2
             ORDER BY updated_ms DESC, key ASC",
        )?;
        let rows = stmt.query_map(params![scope_id, now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (key, kind, status, text, sources_raw, updated_ms, expires_ms) = row?;
            if !include_resolved && status == "resolved" {
                continue;
            }
            let sources: Value = serde_json::from_str(&sources_raw).unwrap_or(json!([]));
            entries.push(json!({
                "key": key,
                "kind": kind,
                "status": status,
                "text": text,
                "sources": sources,
                "updated_ms": updated_ms,
                "expires_ms": expires_ms,
            }));
        }
        Ok(json!({
            "scope_id": scope_id,
            "count": entries.len(),
            "entries": entries,
        }))
    }
}
