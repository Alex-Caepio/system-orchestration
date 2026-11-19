//! Standalone launch adapters for project sessions, system dispatches, and read-only evidence collection.
use crate::{HarnessError, Result, attach, store::SessionStore};
use orch_host::provider::{Launch, LaunchAdapter, PreparedCommand};
use std::{path::PathBuf, sync::Arc};

pub struct WorkspaceLaunchAdapter {
    pub store: SessionStore,
    pub project_id: String,
    pub session_id: Option<String>,
    pub overlay: Option<Arc<attach::OverlayConfig>>,
}

impl LaunchAdapter for WorkspaceLaunchAdapter {
    fn adapt(&self, input: &Launch, mut command: PreparedCommand) -> Result<PreparedCommand> {
        let project = self.store.project(&self.project_id)?;
        let registered = PathBuf::from(
            project["path"]
                .as_str()
                .ok_or_else(|| HarnessError::invalid("Registered project path missing."))?,
        );
        let requested = orch_core::fs::canonicalize(&input.project)?;
        if requested != orch_core::fs::canonicalize(&registered)? {
            let a = orch_core::git::GitFrame::discover(&registered)?.ok_or_else(|| {
                HarnessError::forbidden("Session workspace is outside the registered project.")
            })?;
            let b = orch_core::git::GitFrame::discover(&requested)?.ok_or_else(|| {
                HarnessError::forbidden("Session workspace is outside the registered checkout.")
            })?;
            if a.common_dir()? != b.common_dir()? || a.prefix_bytes != b.prefix_bytes {
                return Err(HarnessError::forbidden(
                    "Session workspace belongs to another project.",
                ));
            }
        }
        if let Some(overlay_cfg) = &self.overlay
            && let Some(overlay) = attach::build_configured_overlay(
                &requested,
                &self.store.state.join("attached").join(&self.project_id),
                &input.provider,
                overlay_cfg,
            )?
        {
            command.argv = attach::apply_overlay(
                &input.provider,
                &command.argv,
                &overlay,
                input.session.is_none(),
            )?;
            for key in ["ORCH_OVERLAY_HOME", "ORCH_STATE_DIR"] {
                if let Some(value) = overlay.environment.get(key) {
                    command.readable.push(PathBuf::from(value));
                }
            }
            command.environment.extend(overlay.environment);
        }
        if input.provider == "claude"
            && let Some(session) = &self.session_id
        {
            let archive = crate::chat_merge::ChatMerge::new(self.store.clone()).directories(session);
            for dir in archive
                .into_iter()
                .chain(crate::attachments::directories(&self.store, session)?)
            {
                command
                    .argv
                    .extend(["--add-dir".into(), dir.to_string_lossy().into_owned()]);
            }
        }
        Ok(command)
    }
}

pub struct SystemLaunchAdapter {
    pub store: SessionStore,
}

impl LaunchAdapter for SystemLaunchAdapter {
    fn adapt(&self, input: &Launch, command: PreparedCommand) -> Result<PreparedCommand> {
        let requested = orch_core::fs::canonicalize(&input.project)?;
        let mut inside = false;
        for project in self.store.projects()? {
            let (Some(id), Some(path)) = (project["id"].as_str(), project["path"].as_str()) else {
                continue;
            };
            let Ok(path) = orch_core::fs::canonicalize(path) else {
                continue;
            };
            if path == requested {
                return WorkspaceLaunchAdapter {
                    store: self.store.clone(),
                    project_id: id.into(),
                    session_id: None,
                    overlay: None,
                }
                .adapt(input, command);
            }
            inside |= requested.starts_with(&path);
        }
        if !inside {
            return Err(HarnessError::forbidden(
                "System dispatch folder is not inside a registered project.",
            ));
        }
        Ok(command)
    }
}

pub struct EvidenceLaunchAdapter {
    pub workspace: PathBuf,
}

impl LaunchAdapter for EvidenceLaunchAdapter {
    fn adapt(&self, input: &Launch, mut command: PreparedCommand) -> Result<PreparedCommand> {
        if input.provider == "claude" {
            command.argv.extend([
                "--add-dir".into(),
                self.workspace.to_string_lossy().into_owned(),
            ]);
        }
        Ok(command)
    }
}
