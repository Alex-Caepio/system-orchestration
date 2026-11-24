//! Standalone subagent write-lock gate and mailbox completion hook handlers.
use crate::counters;
use orch_core::{
    Result, RuntimeError,
    dispatch::{LocalDispatchChannel, validate_task_id},
    fs,
    git::GitFrame,
    publication::PublicationFiles,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct HookOutcome {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn first<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match value {
        Value::Object(map) => map
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| map.values().find_map(|value| first(value, key))),
        Value::Array(items) => items.iter().find_map(|value| first(value, key)),
        _ => None,
    }
}

fn non_empty_str<'a>(val: Option<&'a Value>) -> Option<&'a str> {
    val.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn normalize_host(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "antigravity" | "agy" => "antigravity".to_owned(),
        other => other.to_owned(),
    }
}

fn is_antigravity_payload(payload: &Value) -> bool {
    payload.as_object().is_some_and(|map| {
        map.contains_key("hookEventName")
            || map.contains_key("toolCall")
            || map.contains_key("workspacePaths")
            || map.contains_key("conversationId")
    })
}

fn extract_workspace_root(payload: &Value) -> Option<PathBuf> {
    first(payload, "cwd")
        .or_else(|| {
            payload
                .get("workspacePaths")
                .and_then(Value::as_array)
                .and_then(|items| items.iter().find_map(|v| non_empty_str(Some(v))))
        })
        .map(PathBuf::from)
}

fn extract_tool_name(payload: &Value) -> String {
    non_empty_str(payload.pointer("/toolCall/name"))
        .or_else(|| first(payload, "tool_name"))
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn extract_agent_name(payload: &Value) -> Option<&str> {
    non_empty_str(payload.pointer("/toolCall/args/TypeName"))
        .or_else(|| non_empty_str(payload.pointer("/toolCall/args/Role")))
        .or_else(|| first(payload, "subagent_type"))
        .or_else(|| first(payload, "agent_type"))
        .or_else(|| first(payload, "TypeName"))
        .or_else(|| first(payload, "Role"))
}

fn extract_dispatch_note(payload: &Value) -> String {
    let base = first(payload, "last_assistant_message")
        .or_else(|| first(payload, "status"))
        .or_else(|| non_empty_str(payload.pointer("/toolCall/args/Prompt")))
        .or_else(|| first(payload, "Prompt"))
        .unwrap_or("");
    let normalized = base.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut parts = Vec::new();
    if let Some(ws) = non_empty_str(payload.pointer("/toolCall/args/Workspace")) {
        parts.push(format!("workspace={ws}"));
    }
    if let Some(conv) = first(payload, "conversationId") {
        parts.push(format!("session={conv}"));
    }
    let combined = if parts.is_empty() {
        normalized
    } else if normalized.is_empty() {
        parts.join(" ")
    } else {
        format!("{normalized} ({})", parts.join(", "))
    };
    combined.chars().take(160).collect()
}

fn strings(value: &Value, keys: &[&str]) -> Vec<String> {
    fn visit(value: &Value, keys: &[&str], output: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for key in keys {
                    if let Some(val) = map
                        .get(*key)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        && !output.iter().any(|prior| prior == val)
                    {
                        output.push(val.into());
                    }
                }
                for val in map.values() {
                    visit(val, keys, output);
                }
            }
            Value::Array(items) => {
                for val in items {
                    visit(val, keys, output);
                }
            }
            _ => {}
        }
    }
    let mut output = vec![];
    visit(value, keys, &mut output);
    output
}

fn safe_type(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || " _.:/-".contains(*c))
        .take(64)
        .collect()
}

pub fn project_root(project: &Path) -> PathBuf {
    GitFrame::discover(project)
        .ok()
        .flatten()
        .and_then(|frame| fs::python_realpath_spelling(project, frame.top).ok())
        .unwrap_or_else(|| project.to_path_buf())
}

fn resolve_task_id(project: &Path) -> Option<String> {
    if let Ok(task) = std::env::var("ORCH_TASK_ID")
        .or_else(|_| std::env::var("CONTEXT_TASK_ID"))
        && let Ok(valid) = validate_task_id(&task)
    {
        return Some(valid);
    }
    let frame = GitFrame::discover(project).ok()??;
    frame
        .working_identity()
        .ok()
        .flatten()
        .and_then(|id| validate_task_id(&id).ok())
}

fn roster(project: &Path, host: &str) -> BTreeMap<String, bool> {
    let mut result = BTreeMap::new();
    let dirs: &[&str] = match host {
        "cursor" => &[".cursor/agents"],
        "codex" => &[".codex/agents"],
        "antigravity" => &[".antigravity/agents", ".gemini/agents", ".claude/agents"],
        _ => &[".claude/agents"],
    };
    for dir in dirs {
        let folder = project.join(dir);
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = fs::read_text(&path) else {
                continue;
            };
            let mut lines = text.lines();
            if lines.next() != Some("---") {
                continue;
            }
            let front = lines.take_while(|line| *line != "---").collect::<Vec<_>>();
            let field = |key: &str| {
                front.iter().find_map(|line| {
                    line.strip_prefix(&format!("{key}:"))
                        .map(|value| value.trim().trim_matches('"').to_string())
                })
            };
            if let Some(name) = field("name").filter(|name| !name.is_empty()) {
                result
                    .entry(name)
                    .or_insert_with(|| field("writes").as_deref() == Some("true"));
            }
        }
    }
    result
}

fn gate_reply(host: &str, allow: bool, message: &str) -> Result<HookOutcome> {
    if host == "antigravity" {
        let body = if allow {
            json!({"decision": "allow"})
        } else {
            json!({"decision": "deny", "reason": message})
        };
        Ok(HookOutcome {
            code: 0,
            stdout: format!("{body}\n").into_bytes(),
            stderr: Vec::new(),
        })
    } else if host == "cursor" {
        let body = if allow {
            json!({"permission": "allow"})
        } else {
            json!({"permission": "deny", "user_message": message})
        };
        Ok(HookOutcome {
            code: 0,
            stdout: format!("{body}\n").into_bytes(),
            stderr: Vec::new(),
        })
    } else if allow {
        Ok(HookOutcome::default())
    } else {
        Ok(HookOutcome {
            code: 2,
            stdout: Vec::new(),
            stderr: format!("{message}\n").into_bytes(),
        })
    }
}

pub fn subagent_gate(project: &Path, host: &str, payload: &Value) -> Result<HookOutcome> {
    let root = project_root(project);
    let host = normalize_host(host);
    if host == "codex" {
        let policy_blocks = std::env::var_os("ORCH_BLOCK_BUILTIN_AGENTS").is_some();
        if policy_blocks {
            let forbidden = [
                "spawn_agent",
                "agent",
                "send_input",
                "resume_agent",
                "wait_agent",
                "close_agent",
            ];
            if strings(payload, &["tool_name"])
                .iter()
                .any(|name| forbidden.contains(&name.to_ascii_lowercase().as_str()))
            {
                return gate_reply(
                    &host,
                    false,
                    "BLOCKED: Codex built-in subagents are disabled by orchestration policy in this workspace.",
                );
            }
        }
        return gate_reply(&host, true, "");
    }
    let tool = extract_tool_name(payload);
    if host == "antigravity" {
        if !["invoke_subagent", "agent", "task"].contains(&tool.as_str()) {
            return gate_reply(&host, true, "");
        }
    } else if host != "cursor" && !["agent", "task"].contains(&tool.as_str()) {
        return gate_reply(&host, true, "");
    }
    let Some(agent) = extract_agent_name(payload) else {
        return gate_reply(&host, true, "");
    };
    let roster = roster(&root, &host);
    if roster.is_empty() {
        return gate_reply(&host, true, "");
    }
    let Some(writes) = roster.get(agent) else {
        return gate_reply(&host, true, "");
    };
    if !writes {
        return gate_reply(&host, true, "");
    }
    match counters::take_writer(&root, &host, agent) {
        Ok(None) => gate_reply(&host, true, ""),
        Ok(Some(holder)) => gate_reply(
            &host,
            false,
            &format!(
                "BLOCKED: write-capable agent \"{}\" is already running; wait for completion before starting \"{}\".",
                safe_type(&holder),
                safe_type(agent),
            ),
        ),
        Err(error) => gate_reply(&host, false, &format!("BLOCKED: {error}")),
    }
}

pub fn subagent_dispatch(project: &Path, host: &str, payload: &Value) -> Result<HookOutcome> {
    let root = project_root(project);
    let host = normalize_host(host);
    let relative = ".orch/unrecorded-completions";
    let relay = root.join(relative);
    if host != "antigravity" && first(payload, "hook_event_name") == Some("PostToolUse") {
        if !relay.is_file() {
            return Ok(HookOutcome::default());
        }
        let mut files = PublicationFiles::new(&root)?;
        if files.read(relative, 1024 * 1024)?.is_none() {
            return Ok(HookOutcome::default());
        }
        let taking = root.join(format!("{relative}.relay.{}", orch_core::domain::uuid4()));
        if std::fs::rename(&relay, &taking).is_err() {
            return Ok(HookOutcome::default());
        }
        let current = fs::read_regular(&taking)?;
        let _ = std::fs::remove_file(taking);
        let text = String::from_utf8_lossy(&current);
        let lines = text
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        if lines.is_empty() {
            return Ok(HookOutcome::default());
        }
        let body = json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": lines,
            }
        });
        return Ok(HookOutcome {
            code: 0,
            stdout: format!("{body}\n").into_bytes(),
            stderr: Vec::new(),
        });
    }

    let empty_json_outcome = |stderr: Vec<u8>| HookOutcome {
        code: 0,
        stdout: if host == "antigravity" {
            b"{}\n".to_vec()
        } else {
            Vec::new()
        },
        stderr,
    };

    let Some(agent) = extract_agent_name(payload) else {
        return Ok(empty_json_outcome(Vec::new()));
    };
    let _ = counters::release_writer(&root, agent);
    let Some(task) = resolve_task_id(&root) else {
        return Ok(empty_json_outcome(Vec::new()));
    };
    if !root.join(".orch").is_dir() {
        return Ok(empty_json_outcome(Vec::new()));
    }
    let note = extract_dispatch_note(payload);
    let channel = LocalDispatchChannel::new(&root)?;
    if let Err(err) = channel.dispatch(
        &task,
        agent,
        "complete",
        (!note.is_empty()).then_some(note.as_str()),
        None,
    ) {
        let message = format!(
            "subagent-dispatch: failed to record completion of \"{}\": {err}",
            safe_type(agent),
        );
        if host == "antigravity" {
            return Ok(empty_json_outcome(format!("{message}\n").into_bytes()));
        }
        return Ok(HookOutcome {
            code: 0,
            stdout: format!("{message}\n").into_bytes(),
            stderr: format!("{message}\n").into_bytes(),
        });
    }
    Ok(empty_json_outcome(Vec::new()))
}

pub fn execute_hook(operation: &str, project: Option<&Path>, host: Option<&str>, stdin: &[u8]) -> Result<HookOutcome> {
    let payload: Value = if stdin.iter().all(|b| b.is_ascii_whitespace()) {
        json!({})
    } else {
        serde_json::from_slice(stdin).map_err(|_| RuntimeError::invalid("Hook stdin is not valid JSON"))?
    };
    let resolved_project = project
        .map(Path::to_path_buf)
        .or_else(|| extract_workspace_root(&payload))
        .unwrap_or(std::env::current_dir()?);
    let resolved_host = host
        .map(normalize_host)
        .or_else(|| is_antigravity_payload(&payload).then(|| "antigravity".to_owned()))
        .or_else(|| {
            if std::env::var_os("CLAUDE_PROJECT_DIR").is_some()
                || std::env::var_os("CLAUDE_PLUGIN_ROOT").is_some()
            {
                Some("claude".into())
            } else if std::env::var_os("CODEX_THREAD_ID").is_some() {
                Some("codex".into())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "claude".into());
    match operation {
        "subagent-gate" => subagent_gate(&resolved_project, &resolved_host, &payload),
        "subagent-dispatch" => subagent_dispatch(&resolved_project, &resolved_host, &payload),
        _ => Err(RuntimeError::invalid(format!("Unknown hook operation: {operation}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_subagent_gate_and_dispatch_parse_camelcase_and_enforce_writer_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".antigravity/agents")).unwrap();
        std::fs::write(
            repo.join(".antigravity/agents/php-writer.md"),
            "---\nname: php-writer\nwrites: true\n---\n\nWrite agent\n",
        )
        .unwrap();

        let payload = serde_json::to_vec(&json!({
            "hookEventName": "PreToolUse",
            "conversationId": "conv-12345",
            "workspacePaths": [&repo],
            "toolCall": {
                "name": "invoke_subagent",
                "args": {
                    "TypeName": "php-writer",
                    "Prompt": "Update controller",
                    "Workspace": "inherit"
                }
            }
        }))
        .unwrap();

        // Non-subagent Antigravity tool (`run_command`) always returns allow JSON.
        let run_cmd_payload = serde_json::to_vec(&json!({
            "hookEventName": "PreToolUse",
            "workspacePaths": [&repo],
            "toolCall": { "name": "run_command", "args": { "CommandLine": "git status" } }
        }))
        .unwrap();
        let allow_run = execute_hook("subagent-gate", None, Some("agy"), &run_cmd_payload).unwrap();
        assert_eq!(allow_run.code, 0);
        assert_eq!(
            serde_json::from_slice::<Value>(&allow_run.stdout).unwrap(),
            json!({ "decision": "allow" })
        );

        // First write-capable invoke_subagent succeeds with decision: allow (auto-detected host).
        let first_gate = execute_hook("subagent-gate", None, None, &payload).unwrap();
        assert_eq!(first_gate.code, 0);
        assert_eq!(
            serde_json::from_slice::<Value>(&first_gate.stdout).unwrap(),
            json!({ "decision": "allow" })
        );

        // Concurrent second invoke_subagent returns exit 0 with decision: deny + reason.
        let second_gate = execute_hook("subagent-gate", None, Some("antigravity"), &payload).unwrap();
        assert_eq!(second_gate.code, 0);
        let deny_val: Value = serde_json::from_slice(&second_gate.stdout).unwrap();
        assert_eq!(deny_val["decision"], "deny");
        assert!(
            deny_val["reason"]
                .as_str()
                .unwrap()
                .contains("php-writer")
        );

        // PostToolUse subagent-dispatch releases lock and emits `{}` on stdout with exit 0.
        let post_payload = serde_json::to_vec(&json!({
            "hookEventName": "PostToolUse",
            "conversationId": "conv-12345",
            "workspacePaths": [&repo],
            "toolCall": {
                "name": "invoke_subagent",
                "args": {
                    "Role": "php-writer",
                    "Prompt": "Updated controller",
                    "Workspace": "inherit"
                }
            }
        }))
        .unwrap();
        let post = execute_hook("subagent-dispatch", None, Some("antigravity"), &post_payload).unwrap();
        assert_eq!(post.code, 0);
        assert_eq!(
            serde_json::from_slice::<Value>(&post.stdout).unwrap(),
            json!({})
        );

        // Lock released -> subsequent invoke_subagent allowed again!
        let third_gate = execute_hook("subagent-gate", None, Some("antigravity"), &payload).unwrap();
        assert_eq!(third_gate.code, 0);
        assert_eq!(
            serde_json::from_slice::<Value>(&third_gate.stdout).unwrap(),
            json!({ "decision": "allow" })
        );
        // Clean up the writer lock released by subagent-dispatch.
        let _ = execute_hook("subagent-dispatch", None, Some("antigravity"), &post_payload).unwrap();
    }
}
