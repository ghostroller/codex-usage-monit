use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{CollectionStats, Snapshot, SourceStatus};
use crate::history::HistoryData;
use crate::service::{RecorderStatusFile, ServiceStatus};

pub const HEALTH_REPORT_SCHEMA_VERSION: u32 = 1;

/// A structured view of the health information exposed by the one-shot CLI
/// and the TUI's Other view.
///
/// This deliberately copies only the health-related part of a [`Snapshot`].
/// In particular, it does not serialize the dedicated Codex-home field,
/// task/turn content, or token/account usage merely because those values exist
/// in the snapshot. Preserved source diagnostics may still mention paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    pub schema_version: u32,
    /// Time at which time-sensitive recorder state was evaluated.
    pub as_of: DateTime<Utc>,
    pub snapshot: SnapshotHealth,
    pub history: HistoryHealth,
    pub recorder: RecorderHealth,
    pub service: Option<ServiceStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_error: Option<String>,
}

impl HealthReport {
    pub fn new(
        snapshot: &Snapshot,
        history: &HistoryData,
        recorder_status: Option<&RecorderStatusFile>,
        recorder_error: Option<&str>,
        service_status: Option<&ServiceStatus>,
        now: DateTime<Utc>,
    ) -> Self {
        Self::new_with_service_error(
            snapshot,
            history,
            recorder_status,
            recorder_error,
            service_status,
            None,
            now,
        )
    }

    pub fn new_with_service_error(
        snapshot: &Snapshot,
        history: &HistoryData,
        recorder_status: Option<&RecorderStatusFile>,
        recorder_error: Option<&str>,
        service_status: Option<&ServiceStatus>,
        service_error: Option<&str>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            schema_version: HEALTH_REPORT_SCHEMA_VERSION,
            as_of: now,
            snapshot: SnapshotHealth::from(snapshot),
            history: HistoryHealth::from(history),
            recorder: RecorderHealth::new(recorder_status, recorder_error, now),
            service: service_status.cloned(),
            service_error: service_error.map(str::to_string),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotHealth {
    pub schema_version: u32,
    pub as_of: DateTime<Utc>,
    pub partial: bool,
    pub sources: Vec<SourceStatus>,
    pub stats: CollectionStats,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl From<&Snapshot> for SnapshotHealth {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            schema_version: snapshot.schema_version,
            as_of: snapshot.as_of,
            partial: snapshot.partial,
            sources: snapshot.sources.clone(),
            stats: snapshot.stats.clone(),
            warnings: snapshot.warnings.clone(),
            errors: snapshot.errors.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryHealth {
    pub warnings: Vec<String>,
    pub read_only: bool,
}

impl From<&HistoryData> for HistoryHealth {
    fn from(history: &HistoryData) -> Self {
        Self {
            warnings: history.warnings.clone(),
            read_only: history.read_only,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderHealthState {
    Idle,
    Running,
    Stale,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderHealth {
    pub state: RecorderHealthState,
    pub status: Option<RecorderStatusFile>,
    /// Error encountered while reading recorder status. This is separate from
    /// `status.lastError`, which records the recorder's most recent run error.
    pub error: Option<String>,
}

impl RecorderHealth {
    pub fn new(
        status: Option<&RecorderStatusFile>,
        error: Option<&str>,
        now: DateTime<Utc>,
    ) -> Self {
        let state = if error.is_some() || status.is_some_and(|status| status.last_error.is_some()) {
            RecorderHealthState::Error
        } else if let Some(status) = status {
            if status.heartbeat_is_recent(now) {
                RecorderHealthState::Running
            } else {
                RecorderHealthState::Stale
            }
        } else {
            RecorderHealthState::Idle
        };
        Self {
            state,
            status: status.cloned(),
            error: error.map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{Duration, TimeZone};
    use serde_json::json;

    use super::*;
    use crate::domain::{ApiPricingMetadata, AttributionSummary};
    use crate::service::ServiceState;

    fn snapshot(now: DateTime<Utc>) -> Snapshot {
        Snapshot {
            schema_version: 2,
            api_pricing: ApiPricingMetadata::default(),
            api_equivalent_cost: None,
            as_of: now,
            partial: true,
            codex_home: PathBuf::from("/private/codex-home-must-not-leak"),
            sources: vec![SourceStatus {
                source: "rollout_jsonl".to_string(),
                status: "partial".to_string(),
                as_of: now,
                message: Some("source diagnostic\nwith control".to_string()),
            }],
            limits: Vec::new(),
            rate_limit_reset_credits: None,
            rate_limit_reset_credits_partial: false,
            account_usage: None,
            tasks: Vec::new(),
            turns: Vec::new(),
            models: Vec::new(),
            attribution: AttributionSummary::default(),
            window_analyses: Vec::new(),
            stats: CollectionStats {
                discovered_files: 3,
                scanned_files: 2,
                unreadable_files: 1,
                ..CollectionStats::default()
            },
            warnings: vec!["snapshot warning".to_string()],
            errors: vec!["snapshot error".to_string()],
        }
    }

    #[test]
    fn report_combines_health_sources_with_stable_json_names() {
        let now = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        let snapshot = snapshot(now);
        let history = HistoryData {
            warnings: vec!["history warning".to_string()],
            read_only: true,
            ..HistoryData::default()
        };
        let mut recorder = RecorderStatusFile::started(now, "history-namespace".to_string());
        recorder.record_degraded(now, "recorder error");
        let service = ServiceStatus {
            platform: "linux-systemd-user".to_string(),
            state: ServiceState::Stopped,
            installed: true,
            running: false,
            registration_path: Some(PathBuf::from("/tmp/recorder.service")),
            last_history_heartbeat: Some(now),
            heartbeat_recent: true,
            detail: "service detail".to_string(),
        };

        let report = HealthReport::new(
            &snapshot,
            &history,
            Some(&recorder),
            Some("status read error"),
            Some(&service),
            now,
        );
        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(
            value,
            json!({
                "schemaVersion": 1,
                "asOf": "2026-08-30T12:00:00Z",
                "snapshot": {
                    "schemaVersion": 2,
                    "asOf": "2026-08-30T12:00:00Z",
                    "partial": true,
                    "sources": [{
                        "source": "rollout_jsonl",
                        "status": "partial",
                        "asOf": "2026-08-30T12:00:00Z",
                        "message": "source diagnostic\nwith control"
                    }],
                    "stats": {
                        "discoveredFiles": 3,
                        "scannedFiles": 2,
                        "truncatedFiles": 0,
                        "unreadableFiles": 1,
                        "parsedLines": 0,
                        "skippedLines": 0,
                        "ambiguousTokenResets": 0
                    },
                    "warnings": ["snapshot warning"],
                    "errors": ["snapshot error"]
                },
                "history": {
                    "warnings": ["history warning"],
                    "readOnly": true
                },
                "recorder": {
                    "state": "error",
                    "status": {
                        "schemaVersion": 3,
                        "historyNamespace": "history-namespace",
                        "pid": recorder.pid,
                        "startedAt": "2026-08-30T12:00:00Z",
                        "lastAttemptAt": "2026-08-30T12:00:00Z",
                        "lastHistoryHeartbeat": "2026-08-30T12:00:00Z",
                        "lastError": "recorder error",
                        "historyBackend": "legacy_v1"
                    },
                    "error": "status read error"
                },
                "service": {
                    "platform": "linux-systemd-user",
                    "state": "stopped",
                    "installed": true,
                    "running": false,
                    "registrationPath": "/tmp/recorder.service",
                    "lastHistoryHeartbeat": "2026-08-30T12:00:00Z",
                    "heartbeatRecent": true,
                    "detail": "service detail"
                }
            })
        );

        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains("codex-home-must-not-leak"));
        assert!(!encoded.contains('\n'));
        assert_eq!(
            serde_json::from_str::<HealthReport>(&encoded).unwrap(),
            report
        );
    }

    #[test]
    fn recorder_state_matches_tui_health_semantics() {
        let now = Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap();
        assert_eq!(
            RecorderHealth::new(None, None, now).state,
            RecorderHealthState::Idle
        );
        assert_eq!(
            RecorderHealth::new(None, Some("unreadable"), now).state,
            RecorderHealthState::Error
        );

        let mut recorder = RecorderStatusFile::started(now, "history-namespace".to_string());
        recorder.record_success(now);
        assert_eq!(
            RecorderHealth::new(Some(&recorder), None, now).state,
            RecorderHealthState::Running
        );
        assert_eq!(
            RecorderHealth::new(Some(&recorder), None, now + Duration::minutes(20)).state,
            RecorderHealthState::Stale
        );
        recorder.record_error(now, "collection failed");
        assert_eq!(
            RecorderHealth::new(Some(&recorder), None, now).state,
            RecorderHealthState::Error
        );
    }
}
