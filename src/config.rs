use std::env;
use std::path::PathBuf;
use std::time::Duration;

use crate::startup::StartupTrace;

#[derive(Clone, Debug)]
pub struct CollectConfig {
    pub codex_home: PathBuf,
    pub rollout_cache_dir: Option<PathBuf>,
    pub lookback_days: i64,
    pub max_files: usize,
    pub active_grace: Duration,
    pub redact_content: bool,
    pub offline: bool,
    pub app_server_timeout: Duration,
    pub startup_trace: StartupTrace,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            codex_home: default_codex_home(),
            rollout_cache_dir: None,
            lookback_days: 7,
            max_files: 500,
            active_grace: Duration::from_secs(5 * 60),
            redact_content: false,
            offline: false,
            app_server_timeout: Duration::from_secs(12),
            startup_trace: StartupTrace::default(),
        }
    }
}

pub fn default_codex_home() -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}
