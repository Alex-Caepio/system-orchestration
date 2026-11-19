//! Standalone attached-mode overlay injection without PHP edition coupling.
use crate::{HarnessError, Result};
use orch_core::{fs as safe, publication::PublicationFiles};
use orch_host::provider;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OverlayConfig {
    pub overlay_dir: Option<PathBuf>,
    pub system_prompt: Option<String>,
    pub hooks: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Overlay {
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub settings: Value,
    pub prompt_prefix: String,
}

fn project_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| HarnessError::invalid("Attached project path is not UTF-8."))
}

pub fn project_key(project: &Path) -> Result<String> {
    let canonical = safe::canonicalize(project)?;
    let mut spelling = project_text(&canonical)?;
    if cfg!(windows) {
        spelling = spelling.replace('/', "\\").to_lowercase();
    }
    Ok(orch_core::query::sha256(spelling.as_bytes())[..16].into())
}

pub fn state_directory(project: &Path, base: Option<&Path>) -> Result<PathBuf> {
    let root = match base {
        Some(base) => safe::absolute(base)?,
        None => crate::desktop::default_state()?,
    };
    Ok(root.join("attached").join(project_key(project)?))
}

fn toml_string(value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(serde_json::to_string(text)?),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Array(items) => Ok(format!(
            "[{}]",
            items
                .iter()
                .map(toml_string)
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        )),
        Value::Object(map) => Ok(format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| Ok(format!("{} = {}", serde_json::to_string(k)?, toml_string(v)?)))
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        )),
        Value::Null => Err(HarnessError::invalid("TOML rejects null settings.")),
    }
}

pub fn build_configured_overlay(
    project: &Path,
    state: &Path,
    host: &str,
    config: &OverlayConfig,
) -> Result<Option<Overlay>> {
    if config.overlay_dir.is_none() && config.system_prompt.is_none() && config.hooks.is_none() {
        return Ok(None);
    }
    let project = safe::canonicalize(project)?;
    let state = safe::absolute(state)?;
    if state == project || state.starts_with(&project) {
        return Err(HarnessError::forbidden(
            "Attached state must stay outside the target project.",
        ));
    }
    safe::ensure_directory(&state, true)?;
    let mut files = PublicationFiles::new(&state)?;
    let engine = std::env::current_exe()?;
    let mut environment = BTreeMap::new();
    environment.insert("ORCH_PROJECT_DIR".into(), project_text(&project)?);
    environment.insert("ORCH_STATE_DIR".into(), project_text(&state)?);
    if let Some(dir) = &config.overlay_dir {
        let dir = safe::canonicalize(dir)?;
        environment.insert("ORCH_OVERLAY_HOME".into(), project_text(&dir)?);
    }

    let mut prompt = String::new();
    if let Some(text) = &config.system_prompt {
        prompt.push_str(text);
    }
    if let Some(dir) = &config.overlay_dir {
        for candidate in ["AGENTS.md", "CLAUDE.md"] {
            let file = dir.join(candidate);
            if file.is_file() {
                if !prompt.is_empty() {
                    prompt.push_str("\n\n");
                }
                prompt.push_str(&safe::read_text(&file)?);
                break;
            }
        }
    }

    let mut overlay = Overlay {
        arguments: vec![],
        environment,
        settings: json!({}),
        prompt_prefix: String::new(),
    };

    match host {
        "claude" => {
            if let Some(dir) = &config.overlay_dir {
                overlay.arguments.extend([
                    "--add-dir".into(),
                    project_text(&safe::canonicalize(dir)?)?,
                ]);
            }
            if !prompt.is_empty() {
                let relative = "launch/claude-overlay-prompt.md";
                let previous = files.read(relative, 8 * 1024 * 1024)?;
                if previous.as_deref() != Some(prompt.as_bytes()) {
                    files.write_expected_mode(
                        relative,
                        prompt.as_bytes(),
                        previous.as_deref(),
                        0o600,
                        8 * 1024 * 1024,
                    )?;
                }
                overlay.arguments.extend([
                    "--append-system-prompt-file".into(),
                    project_text(&state.join(relative))?,
                ]);
            }
            let engine_str = project_text(&engine)?;
            let mut settings = json!({
                "permissions": {
                    "allow": [
                        format!("Read(//{}/**)", project_text(&state)?.trim_start_matches('/')),
                        format!("Bash({engine_str} *)"),
                    ],
                    "deny": []
                }
            });
            if let Some(hooks) = &config.hooks {
                settings["hooks"] = hooks.clone();
            } else {
                settings["hooks"] = default_hooks(&engine_str, "claude");
            }
            overlay.settings = settings;
        }
        "codex" => {
            if !prompt.is_empty() {
                overlay.arguments.extend([
                    "-c".into(),
                    format!("developer_instructions={}", toml_string(&json!(prompt))?),
                ]);
            }
            let hooks = config
                .hooks
                .clone()
                .unwrap_or_else(|| default_hooks(&engine.to_string_lossy(), "codex"));
            if let Some(obj) = hooks.as_object() {
                for (event, groups) in obj {
                    overlay.arguments.extend([
                        "-c".into(),
                        format!("hooks.{event}={}", toml_string(groups)?),
                    ]);
                }
            }
            overlay
                .arguments
                .extend(["--add-dir".into(), project_text(&state)?]);
        }
        "cursor" => {
            if let Some(dir) = &config.overlay_dir
                && dir.join(".cursor-plugin/plugin.json").is_file()
            {
                overlay
                    .arguments
                    .extend(["--plugin-dir".into(), project_text(dir)?]);
            }
            if !prompt.is_empty() {
                overlay.prompt_prefix = format!("{prompt}\n\n");
            }
        }
        _ => {
            return Err(HarnessError::invalid(format!(
                "Unsupported attached host: {host}"
            )));
        }
    }
    Ok(Some(overlay))
}

fn default_hooks(engine: &str, host: &str) -> Value {
    let gate = format!("{engine} hook subagent-gate --host {host}");
    let dispatch = format!("{engine} hook subagent-dispatch --host {host}");
    json!({
        "PreToolUse": [{
            "matcher": "Agent|Task",
            "hooks": [{ "type": "command", "command": gate, "timeout": 10 }]
        }],
        "SubagentStop": [{
            "hooks": [{ "type": "command", "command": dispatch, "timeout": 10 }]
        }]
    })
}

pub fn merge_claude(base: &Value, overlay: &Value) -> Result<Value> {
    let mut result = overlay
        .as_object()
        .cloned()
        .ok_or_else(|| HarnessError::invalid("Claude overlay must be an object."))?;
    let base = base
        .as_object()
        .ok_or_else(|| HarnessError::invalid("Claude settings must be an object."))?;
    for (key, value) in base {
        if !["env", "permissions", "hooks"].contains(&key.as_str()) {
            result.insert(key.clone(), value.clone());
        }
    }
    let mut environment = overlay["env"].as_object().cloned().unwrap_or_default();
    if let Some(values) = base.get("env").and_then(Value::as_object) {
        environment.extend(values.clone());
    }
    result.insert("env".into(), Value::Object(environment));
    let mut permissions = serde_json::Map::new();
    for source in [&overlay["permissions"], base.get("permissions").unwrap_or(&Value::Null)] {
        if let Some(object) = source.as_object() {
            for (key, rules) in object {
                let entries = permissions
                    .entry(key.clone())
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .ok_or_else(|| HarnessError::invalid("Permission rules must be arrays."))?;
                for rule in rules
                    .as_array()
                    .ok_or_else(|| HarnessError::invalid("Permission rules must be arrays."))?
                {
                    if !entries.contains(rule) {
                        entries.push(rule.clone());
                    }
                }
            }
        }
    }
    result.insert("permissions".into(), Value::Object(permissions));
    let mut hooks = overlay["hooks"].as_object().cloned().unwrap_or_default();
    if let Some(values) = base.get("hooks").and_then(Value::as_object) {
        for (event, groups) in values {
            hooks
                .entry(event.clone())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| HarnessError::invalid("Hook groups must be arrays."))?
                .extend(
                    groups
                        .as_array()
                        .ok_or_else(|| HarnessError::invalid("Hook groups must be arrays."))?
                        .clone(),
                );
        }
    }
    result.insert("hooks".into(), Value::Object(hooks));
    Ok(Value::Object(result))
}

pub fn apply_overlay(
    host: &str,
    command: &[String],
    overlay: &Overlay,
    first_turn: bool,
) -> Result<Vec<String>> {
    let mut command = command.to_vec();
    match host {
        "claude" => {
            if let Some(idx) = command.iter().position(|a| a == "--settings") {
                let current_raw = command.get(idx + 1).ok_or_else(|| {
                    HarnessError::invalid("Claude settings flag is missing its argument.")
                })?;
                let base: Value = serde_json::from_str(current_raw)?;
                let merged = merge_claude(&base, &overlay.settings)?;
                command[idx + 1] = serde_json::to_string(&merged)?;
            } else {
                command.extend(["--settings".into(), serde_json::to_string(&overlay.settings)?]);
            }
            command.extend(overlay.arguments.clone());
        }
        "codex" => {
            let mut prefix = overlay.arguments.clone();
            if first_turn {
                command.splice(1..1, prefix);
            } else {
                prefix.retain(|a| a != "--add-dir");
                command.splice(1..1, prefix);
            }
        }
        "cursor" => {
            command.extend(overlay.arguments.clone());
        }
        _ => {}
    }
    Ok(command)
}

pub fn run_attached_project(
    project: &Path,
    host: &str,
    overlay_dir: Option<&Path>,
    state_base: Option<&Path>,
    extra_args: &[String],
) -> Result<i32> {
    let project = safe::canonicalize(project)?;
    let state = state_directory(&project, state_base)?;
    let config = OverlayConfig {
        overlay_dir: overlay_dir.map(Path::to_path_buf),
        system_prompt: None,
        hooks: None,
    };
    let overlay = build_configured_overlay(&project, &state, host, &config)?.unwrap_or(Overlay {
        arguments: vec![],
        environment: BTreeMap::from([
            ("ORCH_PROJECT_DIR".into(), project_text(&project)?),
            ("ORCH_STATE_DIR".into(), project_text(&state)?),
        ]),
        settings: json!({}),
        prompt_prefix: String::new(),
    });
    let binary = provider::find_executable(match host {
        "cursor" => "cursor-agent",
        other => other,
    })
    .ok_or_else(|| HarnessError::missing(format!("Provider CLI `{host}` not found in PATH.")))?;
    let mut argv = vec![binary.to_string_lossy().into_owned()];
    argv.extend_from_slice(extra_args);
    let final_argv = apply_overlay(host, &argv, &overlay, true)?;
    let status = Command::new(&final_argv[0])
        .args(&final_argv[1..])
        .current_dir(&project)
        .envs(&overlay.environment)
        .status()?;
    Ok(status.code().unwrap_or(1))
}
