//! Server configuration, runtime control, and XDG/platform state paths.
use crate::{HarnessError, Result, services::Services};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
};

#[derive(Clone)]
pub struct ServerConfig {
    pub source: PathBuf,
    pub state: PathBuf,
    pub port: u16,
    pub timeout: u64,
    pub session_concurrency: usize,
    pub projects: Vec<PathBuf>,
    pub providers: BTreeMap<String, String>,
    pub path_env: Option<std::ffi::OsString>,
}

pub type FactoryFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Arc<dyn Services>>> + Send + 'a>>;

pub trait ServicesFactory: Send + Sync {
    fn create<'a>(
        &'a self,
        config: &'a ServerConfig,
        control: Arc<ServerControl>,
    ) -> FactoryFuture<'a>;
}

pub struct ServerControl {
    pub config: ServerConfig,
    pub signal: tokio::sync::watch::Sender<bool>,
}

fn home() -> Result<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| HarnessError::unavailable("User home directory is unavailable."))
}

pub fn config_directory() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or(home()?.join("AppData/Roaming"))
            .join("system-orchestration-harness"))
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(home()?.join("Library/Application Support/system-orchestration-harness"));
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        Ok(std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".config"))
            .join("system-orchestration-harness"))
    }
}

pub fn default_state() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or(home()?.join("AppData/Local"))
            .join("system-orchestration-harness"))
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(home()?.join("Library/Application Support/system-orchestration-harness/state"));
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        Ok(std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".local/state"))
            .join("system-orchestration-harness"))
    }
}
