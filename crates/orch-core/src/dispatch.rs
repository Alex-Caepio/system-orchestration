//! Standalone append-only orchestration mailbox and dispatch-capsule journal.
//! Persisted inside `.orch/messages/<task_id>.jsonl` under explicit root custody.
use crate::{Result, RuntimeError, capsule, domain, fs, publication::PublicationFiles};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct MessageInput {
    pub from_actor: String,
    pub to_actor: String,
    pub message_type: String,
    pub body: String,
    pub refs: Vec<String>,
    pub capsule_digest: Option<String>,
}

pub fn validate_task_id(task: &str) -> Result<String> {
    let trimmed = task.trim();
    if trimmed.is_empty()
        || trimmed.len() > 128
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(RuntimeError::invalid(
            "Task ID must contain 1..128 ASCII letters, digits, '-', '_', or '.'",
        ));
    }
    Ok(trimmed.to_string())
}

pub fn valid_actor(actor: &str) -> Result<String> {
    let trimmed = actor.trim();
    if trimmed.is_empty()
        || trimmed.len() > 80
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(RuntimeError::invalid(
            "Actor must contain 1..80 ASCII letters, digits, '-', '_', or '.'",
        ));
    }
    Ok(trimmed.to_string())
}

pub struct LocalDispatchChannel {
    pub root: PathBuf,
}

impl LocalDispatchChannel {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = fs::canonicalize(root.into())?;
        Ok(Self { root })
    }

    fn mailbox_relative(&self, task_id: &str) -> Result<String> {
        let id = validate_task_id(task_id)?;
        Ok(format!(".orch/messages/{id}.jsonl"))
    }

    pub fn append_message(&self, task_id: &str, input: MessageInput) -> Result<Value> {
        let rel = self.mailbox_relative(task_id)?;
        domain::guard_shared_text("message body", std::slice::from_ref(&input.body))?;
        fs::ensure_directory(&self.root.join(".orch/messages"), true)?;
        let mut files = PublicationFiles::new(&self.root)?;
        let existing = files.read(&rel, 16 * 1024 * 1024)?;
        let previous_text = existing
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| RuntimeError::invalid("Corrupt UTF-8 mailbox"))?
            .unwrap_or("");
        let seq = previous_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count() as u64
            + 1;
        let entry = json!({
            "seq": seq,
            "id": domain::uuid4(),
            "at": domain::utc_now(),
            "task_id": task_id,
            "from": valid_actor(&input.from_actor)?,
            "to": valid_actor(&input.to_actor)?,
            "type": input.message_type,
            "body": input.body,
            "refs": input.refs,
            "capsule_digest": input.capsule_digest,
        });
        let mut next = previous_text.to_string();
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&serde_json::to_string(&entry)?);
        next.push('\n');
        files.write_expected(&rel, next.as_bytes(), existing.as_deref())?;
        Ok(entry)
    }

    pub fn read_messages(
        &self,
        task_id: &str,
        actor: Option<&str>,
        since: u64,
        kind: Option<&str>,
    ) -> Result<Vec<Value>> {
        let rel = self.mailbox_relative(task_id)?;
        let mut files = PublicationFiles::new(&self.root)?;
        let Some(bytes) = files.read(&rel, 16 * 1024 * 1024)? else {
            return Ok(Vec::new());
        };
        let text =
            std::str::from_utf8(&bytes).map_err(|_| RuntimeError::invalid("Invalid UTF-8"))?;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(line)?;
            let seq = msg["seq"].as_u64().unwrap_or(0);
            if seq <= since {
                continue;
            }
            if let Some(actor) = actor
                && msg["to"].as_str() != Some(actor)
                && msg["from"].as_str() != Some(actor)
            {
                continue;
            }
            if let Some(kind) = kind
                && msg["type"].as_str() != Some(kind)
            {
                continue;
            }
            out.push(msg);
        }
        Ok(out)
    }

    pub fn dispatch(
        &self,
        task_id: &str,
        agent: &str,
        event: &str,
        note: Option<&str>,
        capsule_text: Option<&str>,
    ) -> Result<Value> {
        let agent = valid_actor(agent)?;
        if !["spawn", "complete"].contains(&event) {
            return Err(RuntimeError::invalid(
                "Dispatch event must be spawn or complete",
            ));
        }
        let digest = if let Some(content) = capsule_text {
            let problems = capsule::validate_capsule_text(content)?;
            if !problems.is_empty() && event == "spawn" {
                return Err(RuntimeError::invalid(format!(
                    "Delegation capsule is under-specified: {}",
                    problems.join("; ")
                )));
            }
            Some(domain::hex_lower(Sha256::digest(content.as_bytes())))
        } else {
            None
        };
        let note = note
            .filter(|value| !value.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("{event}: {agent}"));
        domain::guard_shared_text("dispatch note", std::slice::from_ref(&note))?;
        let (from_actor, to_actor, message_type) = if event == "spawn" {
            ("main".into(), agent, "dispatch")
        } else {
            (agent, "main".into(), "completion")
        };
        self.append_message(
            task_id,
            MessageInput {
                from_actor,
                to_actor,
                message_type: message_type.into(),
                body: note.trim().into(),
                refs: Vec::new(),
                capsule_digest: digest,
            },
        )
    }
}

pub fn execute(root: &Path, operation: &str, request: &Value, input: &[u8]) -> Result<Value> {
    let field = |key: &str| {
        request
            .get(key)
            .or_else(|| request.get(key.replace('_', "-")))
            .and_then(Value::as_str)
    };
    let required = |key: &str| {
        field(key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| RuntimeError::invalid(format!("Missing {key}")))
    };
    let content = |path: &str| -> Result<String> {
        if path == "-" {
            String::from_utf8(input.to_vec())
                .map(|text| text.replace("\r\n", "\n").replace('\r', "\n"))
                .map_err(|_| RuntimeError::invalid("Capsule input must be UTF-8"))
        } else {
            fs::read_text(Path::new(path))
        }
    };
    let channel = LocalDispatchChannel::new(root)?;
    match operation {
        "capsule" => capsule::capsule_report(&content(field("file").unwrap_or("-"))?),
        "msg-dispatch" => {
            let capsule_text = field("capsule_file").map(content).transpose()?;
            channel.dispatch(
                required("task_id")?,
                required("agent")?,
                required("event")?,
                field("note"),
                capsule_text.as_deref(),
            )
        }
        "msg-send" => channel.append_message(
            required("task_id")?,
            MessageInput {
                from_actor: field("from").unwrap_or("main").into(),
                to_actor: required("to")?.into(),
                message_type: field("type").unwrap_or("note").into(),
                body: required("body")?.into(),
                refs: request["refs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect(),
                capsule_digest: None,
            },
        ),
        "msg-read" => {
            let messages = channel.read_messages(
                required("task_id")?,
                field("actor"),
                request["since"].as_u64().unwrap_or(0),
                field("type"),
            )?;
            Ok(json!({ "messages": messages }))
        }
        _ => Err(RuntimeError::invalid(format!(
            "Unknown dispatch operation: {operation}"
        ))),
    }
}
