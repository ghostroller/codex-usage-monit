//! Collection inputs and environment-derived defaults, without filesystem I/O.
#![deny(missing_docs)]

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::event_log::EventLog;
use crate::perf::PerfLog;
use crate::startup::StartupTrace;
use crate::trace::TraceLog;

/// Inputs shared by local rollout collection and the optional Codex app-server.
///
/// Construction and cloning do not validate paths, create caches, or start a
/// process. Consumers perform their own I/O and report its failures. Clones
/// share their diagnostic handles. Defaults enable account collection, scan
/// seven days and at most 500 rollout files, and leave persistent caching and
/// all diagnostic sinks disabled.
#[derive(Clone, Debug)]
pub struct CollectConfig {
    /// Codex data directory; also passed as `CODEX_HOME` to the app-server child.
    pub codex_home: PathBuf,
    /// Explicit Codex executable or launcher; `None` uses executable discovery.
    pub codex_bin: Option<PathBuf>,
    /// Override the child process's `PATH`, without changing the current process.
    pub app_server_path: Option<OsString>,
    /// Enable persistent parsed-rollout cache I/O at this directory when set.
    /// `None` still permits the collector's in-memory cache.
    pub rollout_cache_dir: Option<PathBuf>,
    /// Rollout discovery lookback in days; negative values are treated as zero.
    /// This bounds file discovery, not every timestamp inside selected files.
    pub lookback_days: i64,
    /// Maximum rollout files selected for a scan; zero selects no files.
    pub max_files: usize,
    /// Age limit for treating a thread's most recent activity as current.
    pub active_grace: Duration,
    /// Omit content-derived titles and excerpts from collection results and the
    /// current cache namespace. This does not rewrite the original Codex files
    /// or erase existing caches written without redaction.
    pub redact_content: bool,
    /// Skip the Codex app-server account read while still collecting local data.
    pub offline: bool,
    /// Deadline budget used by app-server reads; defaults to 30 seconds.
    /// It does not bound the entire local rollout scan.
    pub app_server_timeout: Duration,
    /// Shared performance sink; its default value is disabled.
    pub perf_log: PerfLog,
    /// Shared content-free diagnostic trace sink; disabled by default.
    pub trace_log: TraceLog,
    /// Shared application diagnostic sink; disabled by default.
    pub event_log: EventLog,
    /// Shared startup timing recorder; disabled by default.
    pub startup_trace: StartupTrace,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            codex_home: default_codex_home(),
            codex_bin: None,
            app_server_path: None,
            rollout_cache_dir: None,
            lookback_days: 7,
            max_files: 500,
            active_grace: Duration::from_secs(5 * 60),
            redact_content: false,
            offline: false,
            app_server_timeout: default_app_server_timeout(cfg!(windows)),
            perf_log: PerfLog::default(),
            trace_log: TraceLog::default(),
            event_log: EventLog::default(),
            startup_trace: StartupTrace::default(),
        }
    }
}

fn default_app_server_timeout(_windows: bool) -> Duration {
    Duration::from_secs(30)
}

/// Resolve `CODEX_HOME`, then `HOME/.codex`, then Windows `USERPROFILE/.codex`.
///
/// If none is set, return the relative path `.codex`. Environment values are
/// used as supplied, including empty or relative paths; no canonicalization,
/// existence check, or directory creation is performed.
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

    #[test]
    fn app_server_timeout_allows_for_network_backed_rate_limit_reads() {
        assert_eq!(default_app_server_timeout(true), Duration::from_secs(30));
        assert_eq!(default_app_server_timeout(false), Duration::from_secs(30));
    }
}
