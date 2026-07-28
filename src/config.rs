use std::env;
use std::path::PathBuf;
use std::time::Duration;

use crate::perf::PerfLog;
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
    pub perf_log: PerfLog,
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
            perf_log: PerfLog::default(),
            startup_trace: StartupTrace::default(),
        }
    }
}

pub fn default_codex_home() -> PathBuf {
    resolve_codex_home(
        env::var_os("CODEX_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
        env::var_os("USERPROFILE").map(PathBuf::from),
        cfg!(windows),
    )
}

fn resolve_codex_home(
    codex_home: Option<PathBuf>,
    home: Option<PathBuf>,
    user_profile: Option<PathBuf>,
    windows: bool,
) -> PathBuf {
    if let Some(codex_home) = codex_home {
        return codex_home;
    }
    home.or_else(|| windows.then_some(user_profile).flatten())
        .map(|home| home.join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_home_resolver_uses_windows_user_profile_as_a_fallback() {
        assert_eq!(
            resolve_codex_home(None, None, Some(PathBuf::from(r"C:\Users\developer")), true,),
            PathBuf::from(r"C:\Users\developer").join(".codex")
        );
        assert_eq!(
            resolve_codex_home(
                None,
                None,
                Some(PathBuf::from(r"C:\Users\developer")),
                false,
            ),
            PathBuf::from(".codex")
        );
    }

    #[test]
    fn codex_home_override_keeps_its_exact_path() {
        let override_path = PathBuf::from("custom-codex-home");
        assert_eq!(
            resolve_codex_home(
                Some(override_path.clone()),
                Some(PathBuf::from("ignored-home")),
                Some(PathBuf::from("ignored-profile")),
                true,
            ),
            override_path
        );
    }
}
