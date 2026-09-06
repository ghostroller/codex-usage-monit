//! Shared invariants for manual and automatic remote-sync attempts.
//!
//! Admission, health recording, and user-facing errors remain caller policy.
//! This module keeps the transport/ledger/fence order identical: exchange,
//! release only a provably pre-transport reservation on failure, settle a
//! successful response, then run the caller's selection fence.

use std::io;

use chrono::{DateTime, Utc};

use crate::history_profile_lease::HistoryProfileLeaseGuard;
use crate::history_runtime::HistoryRuntime;
use crate::remote_bandwidth_budget::{RemoteBandwidthBudgetStore, RemoteBandwidthReservation};
use crate::remote_source_metadata::finalize_remote_source_metadata;
use crate::remote_sync::{
    RemoteDeltaLocalPhases, RemoteDeltaTransport, RemoteSyncError, RemoteSyncHostSnapshot,
    RemoteSyncLimits, RemoteSyncReport, sync_remote_delta_bounded,
};
use crate::remotes_config::RemotesConfigStore;

#[derive(Debug)]
pub(crate) enum RemoteAggregateAttemptError {
    Sync(RemoteSyncError),
    Settlement(io::Error),
    Fence(RemoteSyncError),
}

/// Resources bound to one already-admitted aggregate synchronization attempt.
///
/// Keeping the reservation beside its transport and durable state dependencies
/// makes it difficult for manual and automatic callers to accidentally pair a
/// report with another host's bandwidth admission.
pub(crate) struct AdmittedRemoteAggregateAttempt<'a, L, T> {
    config_store: &'a RemotesConfigStore,
    selected: &'a RemoteSyncHostSnapshot,
    runtime: &'a HistoryRuntime,
    local: &'a mut L,
    transport: &'a mut T,
    bandwidth_budget: &'a RemoteBandwidthBudgetStore,
    reservation: &'a RemoteBandwidthReservation,
}

impl<'a, L, T> AdmittedRemoteAggregateAttempt<'a, L, T>
where
    L: RemoteDeltaLocalPhases,
    T: RemoteDeltaTransport,
{
    pub(crate) fn new(
        config_store: &'a RemotesConfigStore,
        selected: &'a RemoteSyncHostSnapshot,
        runtime: &'a HistoryRuntime,
        local: &'a mut L,
        transport: &'a mut T,
        bandwidth_budget: &'a RemoteBandwidthBudgetStore,
        reservation: &'a RemoteBandwidthReservation,
    ) -> Self {
        Self {
            config_store,
            selected,
            runtime,
            local,
            transport,
            bandwidth_budget,
            reservation,
        }
    }

    /// Runs transport and persistence, settles transferred bytes, then applies
    /// the caller-owned selection fence. Automatic sync uses that fence for
    /// exact host/config eligibility; an explicit manual selection has no
    /// additional automatic-eligibility check.
    pub(crate) fn execute(
        self,
        attempted_at: DateTime<Utc>,
        limits: RemoteSyncLimits,
        completed_at: impl FnOnce() -> DateTime<Utc>,
        after_settlement: impl FnOnce() -> Result<(), RemoteSyncError>,
    ) -> Result<RemoteSyncReport, RemoteAggregateAttemptError> {
        let report = match sync_remote_delta_bounded(
            self.config_store,
            self.selected,
            self.runtime.profile_id().clone(),
            self.local,
            self.transport,
            attempted_at,
            limits,
        ) {
            Ok(report) => report,
            Err(error) => {
                if remote_sync_error_proves_transport_not_started(&error) {
                    let _ = self.bandwidth_budget.cancel_attempt(
                        self.reservation,
                        Utc::now().max(self.reservation.started_at()),
                    );
                }
                return Err(RemoteAggregateAttemptError::Sync(error));
            }
        };

        settle_successful_remote_aggregate(
            self.bandwidth_budget,
            self.reservation,
            &report,
            completed_at().max(self.reservation.started_at()),
            after_settlement,
        )?;
        Ok(report)
    }
}

pub(crate) fn settle_successful_remote_aggregate(
    bandwidth_budget: &RemoteBandwidthBudgetStore,
    reservation: &RemoteBandwidthReservation,
    report: &RemoteSyncReport,
    completed_at: DateTime<Utc>,
    after_settlement: impl FnOnce() -> Result<(), RemoteSyncError>,
) -> Result<(), RemoteAggregateAttemptError> {
    bandwidth_budget
        .complete_report(reservation, completed_at, report)
        .map_err(RemoteAggregateAttemptError::Settlement)?;
    after_settlement().map_err(RemoteAggregateAttemptError::Fence)
}

#[derive(Debug)]
pub(crate) enum RemoteSyncAttemptFinalizeError {
    Profile(io::Error),
    Metadata(io::Error),
}

/// Applies the common final fence only after aggregate and fact work has
/// settled. The profile lease is checked before publishing source metadata.
pub(crate) fn finalize_remote_sync_attempt(
    profile_lease: &HistoryProfileLeaseGuard,
    config_store: &RemotesConfigStore,
    selected: &RemoteSyncHostSnapshot,
    runtime: &HistoryRuntime,
) -> Result<(), RemoteSyncAttemptFinalizeError> {
    profile_lease
        .validate()
        .map_err(RemoteSyncAttemptFinalizeError::Profile)?;
    finalize_remote_source_metadata(config_store, selected, runtime)
        .map_err(RemoteSyncAttemptFinalizeError::Metadata)?;
    Ok(())
}

pub(crate) fn remote_sync_error_proves_transport_not_started(error: &RemoteSyncError) -> bool {
    matches!(
        error,
        RemoteSyncError::HostNotPaired { .. }
            | RemoteSyncError::InvalidLimits(_)
            | RemoteSyncError::PreTransportConfigurationChanged { .. }
            | RemoteSyncError::PreTransportLocal(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_release_contract_is_conservative_after_transport_boundary() {
        assert!(remote_sync_error_proves_transport_not_started(
            &RemoteSyncError::InvalidLimits("invalid limits")
        ));
        assert!(remote_sync_error_proves_transport_not_started(
            &RemoteSyncError::HostNotPaired {
                host_id: "dev".to_owned(),
            }
        ));
        assert!(remote_sync_error_proves_transport_not_started(
            &RemoteSyncError::PreTransportConfigurationChanged {
                host_id: "dev".to_owned(),
            }
        ));
        assert!(remote_sync_error_proves_transport_not_started(
            &RemoteSyncError::PreTransportLocal(io::Error::new(
                io::ErrorKind::WouldBlock,
                "history writer busy",
            ))
        ));
        assert!(!remote_sync_error_proves_transport_not_started(
            &RemoteSyncError::ConfigurationChanged {
                host_id: "dev".to_owned(),
            }
        ));
        assert!(!remote_sync_error_proves_transport_not_started(
            &RemoteSyncError::InvalidStartedAt
        ));
    }
}
