//! Short-lived runtime entry point for the remote usage protocol.
//!
//! Probe, aggregate Delta, and fixed-watermark per-thread fact requests are
//! framed end to end, including durable live replacement revisions.

use std::fs;
use std::io::{self, Read, Write};
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;

use chrono::{DateTime, Utc};

use crate::api_cost::API_PRICING_CATALOG_REVISION;
use crate::config::CollectConfig;
use crate::history::{
    HISTORY_ESTIMATOR_REVISION, HISTORY_FORMAT_VERSION, HISTORY_METRIC_REVISION,
    HISTORY_PROJECT_BREAKDOWN_REVISION,
};
use crate::remote_exporter::{
    PreparedRemoteDeltaPage, RemoteDeltaPrepareError, prepare_remote_delta_page,
    probe_remote_export_state_writable,
};
use crate::remote_fact_exporter::{
    PreparedRemoteFactPage, PreparedRemoteFactPageEnvelope, RemoteFactPrepareError,
    prepare_remote_fact_page,
};
use crate::remote_protocol::{
    DeltaPayload, ProbeResult, ProtocolRevisions, REMOTE_PROTOCOL_VERSION, RemoteCapability,
    RemoteExportRequest, RemoteExportRequestBody, RemoteExportResponseBody, RemoteFailure,
    RemoteFailureKind, RemoteFrameLimits, RemoteProtocolErrorKind, RemoteSessionFactPayload,
    RemoteSessionFactResponse, RemoteTiming, SourceGeneration, encode_remote_response_for_request,
    read_remote_frame,
};
use crate::source_identity::{SourceIdentity, SourceIdentityStore};

type ProtocolResponse = RemoteSessionFactResponse;

pub(crate) fn serve_export<R, W>(
    config: &CollectConfig,
    identity_store: &SourceIdentityStore,
    reader: R,
    writer: &mut W,
) -> anyhow::Result<()>
where
    R: Read,
    W: Write,
{
    let request =
        read_remote_frame::<RemoteExportRequest, _>(reader, RemoteFrameLimits::default())?;
    let received_at = Utc::now();
    let identity = identity_store.load_or_create()?;
    let revisions = current_revisions();
    let source = source_generation(&identity);
    if let Some(expected) = request.expected_source.as_ref()
        && expected != &source
    {
        return write_response_body(
            writer,
            &request,
            &identity,
            &revisions,
            received_at,
            Utc::now(),
            failure(
                RemoteFailureKind::IdentityMismatch,
                "remote source identity or generation does not match the pinned source",
                None,
            ),
        );
    }
    if !request.accepted_revisions.accepts(&revisions) {
        return write_response_body(
            writer,
            &request,
            &identity,
            &revisions,
            received_at,
            Utc::now(),
            failure(
                RemoteFailureKind::VersionMismatch,
                "remote data revisions are outside the accepted ranges",
                None,
            ),
        );
    }

    match &request.request {
        RemoteExportRequestBody::Probe(probe) => {
            let state_writable = !probe.check_state_writable
                || (identity_store.probe_state_directory_writable().is_ok()
                    && probe_remote_export_state_writable(identity_store, &revisions).is_ok()
                    && config
                        .rollout_cache_dir
                        .as_deref()
                        .is_some_and(|directory| {
                            crate::cache::probe_private_directory_writable(directory).is_ok()
                        }));
            let rollout_readable =
                !probe.check_rollout_readable || rollout_roots_are_readable(&config.codex_home);
            write_response_body(
                writer,
                &request,
                &identity,
                &revisions,
                received_at,
                Utc::now(),
                RemoteExportResponseBody::Probe(ProbeResult {
                    capabilities: vec![
                        RemoteCapability::DeltaJournal,
                        RemoteCapability::LiveSnapshot,
                        RemoteCapability::SessionFactSnapshot,
                        RemoteCapability::SessionFactDelta,
                        RemoteCapability::RedactedContent,
                        RemoteCapability::PreviewContent,
                        RemoteCapability::GzipFrame,
                    ],
                    state_writable,
                    rollout_readable,
                }),
            )
        }
        RemoteExportRequestBody::Delta(delta) => {
            let observed_at = Utc::now();
            match prepare_remote_delta_page(
                config,
                identity_store,
                &identity,
                &revisions,
                request.redaction_profile,
                delta,
                observed_at,
            ) {
                Ok(prepared) => write_prepared_delta(
                    writer,
                    &request,
                    &identity,
                    &revisions,
                    received_at,
                    &prepared,
                ),
                Err(error) => write_response_body(
                    writer,
                    &request,
                    &identity,
                    &revisions,
                    received_at,
                    Utc::now(),
                    failure_for_prepare_error(&error),
                ),
            }
        }
        RemoteExportRequestBody::SessionFacts(facts) => {
            let observed_at = Utc::now();
            match prepare_remote_fact_page(
                config,
                identity_store,
                &identity,
                &revisions,
                request.redaction_profile,
                facts,
                observed_at,
            ) {
                Ok(prepared) => write_prepared_fact(
                    writer,
                    &request,
                    &identity,
                    &revisions,
                    received_at,
                    &prepared,
                ),
                Err(error) => write_response_body(
                    writer,
                    &request,
                    &identity,
                    &revisions,
                    received_at,
                    Utc::now(),
                    failure_for_fact_prepare_error(&error),
                ),
            }
        }
    }
}

fn failure(
    kind: RemoteFailureKind,
    message: &'static str,
    retry_after_seconds: Option<u32>,
) -> RemoteExportResponseBody<DeltaPayload, RemoteSessionFactPayload> {
    RemoteExportResponseBody::Failure(RemoteFailure {
        kind,
        message: message.to_owned(),
        retry_after_seconds,
    })
}

fn failure_for_prepare_error(
    error: &RemoteDeltaPrepareError,
) -> RemoteExportResponseBody<DeltaPayload, RemoteSessionFactPayload> {
    match error {
        RemoteDeltaPrepareError::Busy => failure(
            RemoteFailureKind::Busy,
            "remote exporter is busy; retry this source shortly",
            Some(1),
        ),
        RemoteDeltaPrepareError::CursorExpired(_) => failure(
            RemoteFailureKind::CursorExpired,
            "remote delta cursor expired; restart this source with a bootstrap request",
            None,
        ),
        RemoteDeltaPrepareError::Internal(_) => failure(
            RemoteFailureKind::Internal,
            "remote delta export failed before a page could be produced",
            None,
        ),
    }
}

fn failure_for_fact_prepare_error(
    error: &RemoteFactPrepareError,
) -> RemoteExportResponseBody<DeltaPayload, RemoteSessionFactPayload> {
    match error {
        RemoteFactPrepareError::Busy => failure(
            RemoteFailureKind::Busy,
            "remote fact exporter is busy; retry this thread shortly",
            Some(1),
        ),
        RemoteFactPrepareError::FactCursorExpired => failure(
            RemoteFailureKind::FactCursorExpired,
            "remote fact cursor or frozen page expired; restart this thread with a snapshot",
            None,
        ),
        RemoteFactPrepareError::IncompleteScan => failure(
            RemoteFailureKind::FactEvidenceUnavailable,
            "remote fact inventory is unavailable until a complete rollout scan succeeds",
            None,
        ),
        RemoteFactPrepareError::DigestChanged => failure(
            RemoteFailureKind::FactDigestChanged,
            "remote fact digest changed; refresh aggregate evidence before retrying",
            None,
        ),
        RemoteFactPrepareError::InventoryTooLarge => failure(
            RemoteFailureKind::FactInventoryTooLarge,
            "remote fact inventory exceeds the bounded complete-batch limit",
            None,
        ),
        RemoteFactPrepareError::Internal(_) => failure(
            RemoteFailureKind::Internal,
            "remote fact export failed before a page could be produced",
            None,
        ),
    }
}

fn write_prepared_delta<W: Write>(
    writer: &mut W,
    request: &RemoteExportRequest,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    received_at: DateTime<Utc>,
    prepared: &PreparedRemoteDeltaPage,
) -> anyhow::Result<()> {
    let mut entry_limit = prepared.entry_count();
    loop {
        let (page, payload) = match prepared.decode_prefix(entry_limit) {
            Ok(decoded) => decoded,
            Err(error) if error.is_splittable() && entry_limit > 1 => {
                entry_limit = entry_limit.div_ceil(2);
                continue;
            }
            Err(_) => {
                return write_response_body(
                    writer,
                    request,
                    identity,
                    revisions,
                    received_at,
                    Utc::now(),
                    failure(
                        RemoteFailureKind::Internal,
                        "remote delta journal could not be decoded safely",
                        None,
                    ),
                );
            }
        };
        let response = protocol_response(
            request,
            identity,
            revisions,
            received_at,
            prepared.observed_at(),
            RemoteExportResponseBody::Delta { page, payload },
        );
        match encode_remote_response_for_request(&response, request, RemoteFrameLimits::default()) {
            Ok(frame) => return write_complete_frame(writer, &frame),
            Err(error)
                if matches!(
                    error.kind(),
                    RemoteProtocolErrorKind::EncodedLimitExceeded
                        | RemoteProtocolErrorKind::DecodedLimitExceeded
                ) && entry_limit > 1 =>
            {
                entry_limit = entry_limit.div_ceil(2);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    RemoteProtocolErrorKind::EncodedLimitExceeded
                        | RemoteProtocolErrorKind::DecodedLimitExceeded
                ) =>
            {
                return write_response_body(
                    writer,
                    request,
                    identity,
                    revisions,
                    received_at,
                    Utc::now(),
                    failure(
                        RemoteFailureKind::InvalidRequest,
                        "maxPageBytes is too small for the next remote delta record",
                        None,
                    ),
                );
            }
            Err(_) => {
                return write_response_body(
                    writer,
                    request,
                    identity,
                    revisions,
                    received_at,
                    Utc::now(),
                    failure(
                        RemoteFailureKind::Internal,
                        "remote delta response failed protocol validation",
                        None,
                    ),
                );
            }
        }
    }
}

fn write_prepared_fact<W: Write>(
    writer: &mut W,
    request: &RemoteExportRequest,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    received_at: DateTime<Utc>,
    prepared: &PreparedRemoteFactPage,
) -> anyhow::Result<()> {
    let mut entry_limit = match prepared.safe_wire_entry_limit() {
        Ok(limit) => limit,
        Err(_) => {
            return write_response_body(
                writer,
                request,
                identity,
                revisions,
                received_at,
                Utc::now(),
                failure(
                    RemoteFailureKind::Internal,
                    "remote fact batch could not be bounded safely",
                    None,
                ),
            );
        }
    };
    loop {
        let (page, payload) = match prepared.decode_prefix(entry_limit) {
            Ok(decoded) => decoded,
            Err(_) => {
                return write_response_body(
                    writer,
                    request,
                    identity,
                    revisions,
                    received_at,
                    Utc::now(),
                    failure(
                        RemoteFailureKind::Internal,
                        "remote fact batch could not be decoded safely",
                        None,
                    ),
                );
            }
        };
        let result = match page {
            PreparedRemoteFactPageEnvelope::Snapshot(page) => {
                RemoteExportResponseBody::FactSnapshot { page, payload }
            }
            PreparedRemoteFactPageEnvelope::Delta(page) => {
                RemoteExportResponseBody::FactDelta { page, payload }
            }
        };
        let response = protocol_response(
            request,
            identity,
            revisions,
            received_at,
            prepared.observed_at(),
            result,
        );
        match encode_remote_response_for_request(&response, request, RemoteFrameLimits::default()) {
            Ok(frame) => return write_complete_frame(writer, &frame),
            Err(error)
                if matches!(
                    error.kind(),
                    RemoteProtocolErrorKind::EncodedLimitExceeded
                        | RemoteProtocolErrorKind::DecodedLimitExceeded
                ) && entry_limit > 1 =>
            {
                entry_limit = entry_limit.div_ceil(2);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    RemoteProtocolErrorKind::EncodedLimitExceeded
                        | RemoteProtocolErrorKind::DecodedLimitExceeded
                ) =>
            {
                return write_response_body(
                    writer,
                    request,
                    identity,
                    revisions,
                    received_at,
                    Utc::now(),
                    failure(
                        RemoteFailureKind::InvalidRequest,
                        "maxPageBytes is too small for the next remote fact record",
                        None,
                    ),
                );
            }
            Err(_) => {
                return write_response_body(
                    writer,
                    request,
                    identity,
                    revisions,
                    received_at,
                    Utc::now(),
                    failure(
                        RemoteFailureKind::Internal,
                        "remote fact response failed protocol validation",
                        None,
                    ),
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_response_body<W: Write>(
    writer: &mut W,
    request: &RemoteExportRequest,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    received_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    result: RemoteExportResponseBody<DeltaPayload, RemoteSessionFactPayload>,
) -> anyhow::Result<()> {
    let response = protocol_response(
        request,
        identity,
        revisions,
        received_at,
        observed_at,
        result,
    );
    let frame =
        encode_remote_response_for_request(&response, request, RemoteFrameLimits::default())?;
    write_complete_frame(writer, &frame)
}

fn protocol_response(
    request: &RemoteExportRequest,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    received_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    result: RemoteExportResponseBody<DeltaPayload, RemoteSessionFactPayload>,
) -> ProtocolResponse {
    protocol_response_at(
        request,
        identity,
        revisions,
        received_at,
        observed_at,
        Utc::now(),
        result,
    )
}

#[allow(clippy::too_many_arguments)]
fn protocol_response_at(
    request: &RemoteExportRequest,
    identity: &SourceIdentity,
    revisions: &ProtocolRevisions,
    received_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    sent_at: DateTime<Utc>,
    result: RemoteExportResponseBody<DeltaPayload, RemoteSessionFactPayload>,
) -> ProtocolResponse {
    // Wall clocks can move backwards while a short-lived SSH command is
    // running. Clamp all response watermarks monotonically so a clock step can
    // never turn an otherwise useful structured response into empty stdout.
    let observed_at = observed_at.max(received_at);
    let remote_sent_at = sent_at.max(observed_at).max(received_at);
    ProtocolResponse {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        server_version: env!("CARGO_PKG_VERSION")
            .parse()
            .expect("Cargo package versions satisfy the remote version grammar"),
        source: source_generation(identity),
        redaction_profile: request.redaction_profile,
        revisions: revisions.clone(),
        observed_at,
        timing: RemoteTiming {
            remote_received_at: received_at,
            remote_sent_at,
        },
        result,
    }
}

fn write_complete_frame<W: Write>(writer: &mut W, frame: &[u8]) -> anyhow::Result<()> {
    writer.write_all(frame)?;
    Ok(())
}

fn source_generation(identity: &SourceIdentity) -> SourceGeneration {
    SourceGeneration {
        node_id: identity.node_id().clone(),
        generation: NonZeroU64::new(identity.generation())
            .expect("validated source identities have a non-zero generation"),
    }
}

pub(crate) fn current_revisions() -> ProtocolRevisions {
    ProtocolRevisions {
        history_format: nonzero_revision(HISTORY_FORMAT_VERSION),
        metric: nonzero_revision(HISTORY_METRIC_REVISION),
        estimator: nonzero_revision(HISTORY_ESTIMATOR_REVISION),
        project_breakdown: nonzero_revision(HISTORY_PROJECT_BREAKDOWN_REVISION),
        api_pricing_catalog: nonzero_revision(API_PRICING_CATALOG_REVISION),
    }
}

pub(crate) fn current_accepted_revisions() -> crate::remote_protocol::AcceptedRevisions {
    use crate::remote_protocol::{AcceptedRevisionRange, AcceptedRevisions};

    let revisions = current_revisions();
    let exact = |revision| AcceptedRevisionRange {
        min: revision,
        max: revision,
    };
    AcceptedRevisions {
        history_format: exact(revisions.history_format),
        metric: exact(revisions.metric),
        estimator: exact(revisions.estimator),
        project_breakdown: exact(revisions.project_breakdown),
        api_pricing_catalog: exact(revisions.api_pricing_catalog),
    }
}

fn nonzero_revision(revision: u32) -> NonZeroU32 {
    NonZeroU32::new(revision).expect("compile-time protocol revisions must be non-zero")
}

fn rollout_roots_are_readable(codex_home: &Path) -> bool {
    let mut found_root = false;
    for name in ["sessions", "archived_sessions"] {
        match fs::read_dir(codex_home.join(name)) {
            Ok(_) => found_root = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    found_root || fs::read_dir(codex_home).is_ok()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::num::{NonZeroU32, NonZeroU64};

    use chrono::{Duration, Utc};
    use serde_json::json;

    use super::*;
    use crate::remote_protocol::{
        AcceptedRevisionRange, AcceptedRevisions, DeltaRequest, EmptyRemotePayload, ExportRange,
        ProbeRequest, RemoteProtocolMessage, SessionFactsDigestBinding, decode_remote_frame,
        decode_remote_response_for_request, encode_remote_frame,
    };
    use crate::source_history::RedactionProfile;

    fn accepted_revisions() -> AcceptedRevisions {
        let exact = |value| AcceptedRevisionRange {
            min: NonZeroU32::new(value).unwrap(),
            max: NonZeroU32::new(value).unwrap(),
        };
        AcceptedRevisions {
            history_format: exact(HISTORY_FORMAT_VERSION),
            metric: exact(HISTORY_METRIC_REVISION),
            estimator: exact(HISTORY_ESTIMATOR_REVISION),
            project_breakdown: exact(HISTORY_PROJECT_BREAKDOWN_REVISION),
            api_pricing_catalog: exact(API_PRICING_CATALOG_REVISION),
        }
    }

    fn probe_request() -> RemoteExportRequest {
        RemoteExportRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            client_version: "0.4.0-test".parse().unwrap(),
            expected_source: None,
            redaction_profile: RedactionProfile::Redacted,
            max_page_bytes: 1024 * 1024,
            accepted_revisions: accepted_revisions(),
            request: RemoteExportRequestBody::Probe(ProbeRequest {
                check_state_writable: true,
                check_rollout_readable: true,
            }),
        }
    }

    fn expected_fact_digests(
        config: &CollectConfig,
        identity: &SourceIdentity,
        thread_id: &crate::source_model::ThreadId,
        observed_at: DateTime<Utc>,
    ) -> Vec<SessionFactsDigestBinding> {
        let range = ExportRange {
            from: observed_at - Duration::days(35),
            to: observed_at,
        };
        let collection = crate::remote_collection::collect_remote_rollouts(
            config,
            &range,
            observed_at,
            RedactionProfile::Redacted,
        )
        .unwrap();
        let publication = collection.aggregate_publication().unwrap();
        let observation = crate::source_export::source_normalized_observation(
            identity,
            &collection.dataset.tasks,
            publication.observation(),
        );
        let evidence = crate::source_export::materialize_local_session_digest_evidence(
            &collection.dataset.calls,
            &observation.half_hour_buckets,
            observed_at,
            true,
        )
        .unwrap();
        crate::source_export::finalize_local_session_digests(identity, &evidence, &observation)
            .unwrap()
            .into_iter()
            .filter(|digest| {
                digest.replica().thread_id() == thread_id && digest.exact_event_identity()
            })
            .map(|digest| {
                let metrics = digest.metrics();
                SessionFactsDigestBinding {
                    range_start: digest.range_start(),
                    range_end: digest.range_end(),
                    covered_through: digest.covered_through(),
                    coverage_complete: digest.coverage_complete(),
                    fingerprint: digest.fingerprint().as_str().parse().unwrap(),
                    project_breakdown_fingerprint: digest
                        .project_breakdown_fingerprint()
                        .as_str()
                        .parse()
                        .unwrap(),
                    event_count: digest.event_count(),
                    metric_revision: NonZeroU32::new(metrics.metric_revision).unwrap(),
                    estimator_revision: NonZeroU32::new(metrics.estimator_revision).unwrap(),
                    project_breakdown_revision: NonZeroU32::new(metrics.project_breakdown_revision)
                        .unwrap(),
                    api_pricing_catalog_revision: NonZeroU32::new(
                        metrics.api_pricing_catalog_revision,
                    )
                    .unwrap(),
                }
            })
            .collect()
    }

    fn serve(
        config: &CollectConfig,
        store: &SourceIdentityStore,
        request: &RemoteExportRequest,
    ) -> ProtocolResponse {
        let output = serve_frame(config, store, request);
        decode_remote_frame(&output, RemoteFrameLimits::default()).unwrap()
    }

    fn serve_frame(
        config: &CollectConfig,
        store: &SourceIdentityStore,
        request: &RemoteExportRequest,
    ) -> Vec<u8> {
        let input = encode_remote_frame(request, RemoteFrameLimits::default()).unwrap();
        let mut output = Vec::new();
        serve_export(config, store, Cursor::new(input), &mut output).unwrap();
        assert_ne!(output.last(), Some(&b'\n'));
        output
    }

    fn write_rollout(codex_home: &Path, project: &Path, now: DateTime<Utc>) {
        let event_at = now - Duration::hours(2);
        let sessions = codex_home
            .join("sessions")
            .join(event_at.format("%Y/%m/%d").to_string());
        fs::create_dir_all(&sessions).unwrap();
        let records = [
            json!({
                "timestamp": (event_at - Duration::minutes(2)).to_rfc3339(),
                "type": "session_meta",
                "payload": {
                    "id": "01a00000-0000-7000-8000-000000000001",
                    "timestamp": (event_at - Duration::minutes(2)).to_rfc3339(),
                    "cwd": project
                }
            }),
            json!({
                "timestamp": (event_at - Duration::minutes(1)).to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "turn-1"}
            }),
            json!({
                "timestamp": (event_at - Duration::minutes(1)).to_rfc3339(),
                "type": "turn_context",
                "payload": {"turn_id": "turn-1", "model": "gpt-5.6-sol"}
            }),
            json!({
                "timestamp": event_at.to_rfc3339(),
                "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": {
                    "input_tokens": 80,
                    "cached_input_tokens": 40,
                    "output_tokens": 20,
                    "reasoning_output_tokens": 10,
                    "total_tokens": 100
                }}}
            }),
        ];
        let contents = records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(sessions.join("rollout-remote.jsonl"), contents).unwrap();
    }

    #[test]
    fn probe_reports_only_implemented_capabilities_without_scanning_rollouts() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("codex/sessions")).unwrap();
        fs::write(
            directory.path().join("codex/sessions/not-a-rollout"),
            b"this must never be parsed",
        )
        .unwrap();
        let config = CollectConfig {
            codex_home: directory.path().join("codex"),
            rollout_cache_dir: Some(directory.path().join("cache")),
            ..CollectConfig::default()
        };
        let identity_path = directory.path().join("state/source-identity.json");
        let store = SourceIdentityStore::at_path(identity_path.clone());
        let request = probe_request();

        let response = serve(&config, &store, &request);
        response.validate_for_request(&request).unwrap();
        let RemoteExportResponseBody::Probe(probe) = &response.result else {
            panic!("expected probe response");
        };
        assert_eq!(
            probe.capabilities,
            vec![
                RemoteCapability::DeltaJournal,
                RemoteCapability::LiveSnapshot,
                RemoteCapability::SessionFactSnapshot,
                RemoteCapability::SessionFactDelta,
                RemoteCapability::RedactedContent,
                RemoteCapability::PreviewContent,
                RemoteCapability::GzipFrame,
            ]
        );
        assert!(probe.state_writable);
        assert!(probe.rollout_readable);

        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(identity_path).unwrap()).unwrap();
        let secret = persisted["projectKeySecret"].as_str().unwrap();
        let serialized = serde_json::to_vec(&response).unwrap();
        assert!(
            !serialized
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_requires_both_identity_state_and_rollout_cache_to_be_writable() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let state_directory = directory.path().join("state");
        let identity_path = state_directory.join("source-identity.json");
        let store = SourceIdentityStore::at_path(identity_path);
        store.load_or_create().unwrap();
        let cache_directory = directory.path().join("cache");
        crate::cache::probe_private_directory_writable(&cache_directory).unwrap();
        let config = CollectConfig {
            rollout_cache_dir: Some(cache_directory.clone()),
            ..CollectConfig::default()
        };
        let mut request = probe_request();
        if let RemoteExportRequestBody::Probe(probe) = &mut request.request {
            probe.check_rollout_readable = false;
        }

        fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o500)).unwrap();
        let response = serve(&config, &store, &request);
        fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let RemoteExportResponseBody::Probe(probe) = response.result else {
            panic!("expected probe response");
        };
        assert!(!probe.state_writable);

        let exporter_directory =
            crate::remote_exporter::revision_bound_state_root(&store, &current_revisions())
                .unwrap();
        crate::cache::probe_private_directory_writable(&exporter_directory).unwrap();
        fs::set_permissions(&exporter_directory, fs::Permissions::from_mode(0o500)).unwrap();
        let response = serve(&config, &store, &request);
        fs::set_permissions(&exporter_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let RemoteExportResponseBody::Probe(probe) = response.result else {
            panic!("expected probe response");
        };
        assert!(!probe.state_writable);

        fs::set_permissions(&cache_directory, fs::Permissions::from_mode(0o500)).unwrap();
        let response = serve(&config, &store, &request);
        fs::set_permissions(&cache_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let RemoteExportResponseBody::Probe(probe) = response.result else {
            panic!("expected probe response");
        };
        assert!(!probe.state_writable);
    }

    #[test]
    fn empty_but_readable_codex_home_is_a_valid_rollout_source() {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        fs::create_dir(&codex_home).unwrap();
        assert!(rollout_roots_are_readable(&codex_home));
        assert!(!rollout_roots_are_readable(
            &directory.path().join("missing")
        ));
    }

    #[test]
    fn pinned_identity_mismatch_is_a_structured_failure() {
        let directory = tempfile::tempdir().unwrap();
        let config = CollectConfig::default();
        let store =
            SourceIdentityStore::at_path(directory.path().join("state/source-identity.json"));
        let mut request = probe_request();
        request.expected_source = Some(SourceGeneration {
            node_id: "node-11111111111111111111111111111111".parse().unwrap(),
            generation: NonZeroU64::new(1).unwrap(),
        });

        let response = serve(&config, &store, &request);
        assert!(matches!(
            response.result,
            RemoteExportResponseBody::Failure(RemoteFailure {
                kind: RemoteFailureKind::IdentityMismatch,
                ..
            })
        ));
    }

    #[test]
    fn fact_evidence_failures_have_structured_retry_semantics() {
        for (error, expected) in [
            (
                RemoteFactPrepareError::IncompleteScan,
                RemoteFailureKind::FactEvidenceUnavailable,
            ),
            (
                RemoteFactPrepareError::DigestChanged,
                RemoteFailureKind::FactDigestChanged,
            ),
            (
                RemoteFactPrepareError::InventoryTooLarge,
                RemoteFailureKind::FactInventoryTooLarge,
            ),
        ] {
            let RemoteExportResponseBody::Failure(failure) = failure_for_fact_prepare_error(&error)
            else {
                panic!("fact preparation failures must stay structured")
            };
            assert_eq!(failure.kind, expected);
            assert!(failure.retry_after_seconds.is_none());
        }
    }

    #[test]
    fn response_timestamps_are_monotonic_across_a_wall_clock_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            SourceIdentityStore::at_path(directory.path().join("state/source-identity.json"));
        let identity = store.load_or_create().unwrap();
        let request = probe_request();
        let received_at = Utc::now();
        let response = protocol_response_at(
            &request,
            &identity,
            &current_revisions(),
            received_at,
            received_at - Duration::seconds(2),
            received_at - Duration::seconds(1),
            RemoteExportResponseBody::Probe(ProbeResult {
                capabilities: vec![RemoteCapability::GzipFrame],
                state_writable: true,
                rollout_readable: true,
            }),
        );

        assert_eq!(response.observed_at, received_at);
        assert_eq!(response.timing.remote_sent_at, received_at);
        response.validate_for_request(&request).unwrap();
    }

    #[test]
    fn live_delta_returns_a_revisioned_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let config = CollectConfig {
            codex_home: directory.path().join("codex"),
            rollout_cache_dir: Some(directory.path().join("cache")),
            ..CollectConfig::default()
        };
        let store =
            SourceIdentityStore::at_path(directory.path().join("state/source-identity.json"));
        let mut request = probe_request();
        request.expected_source = Some(source_generation(&store.load_or_create().unwrap()));
        let now = Utc::now();
        request.request = RemoteExportRequestBody::Delta(DeltaRequest {
            delta_cursor: None,
            range: ExportRange {
                from: now - Duration::hours(1),
                to: now,
            },
            overlap_minutes: 60,
            include_live: true,
            known_live_revision: None,
        });
        request.validate_remote_protocol().unwrap();

        let response = serve(&config, &store, &request);
        let RemoteExportResponseBody::Delta { payload, .. } = response.result else {
            panic!("expected live delta response")
        };
        assert!(payload.live.and_then(|live| live.snapshot).is_some());
    }

    #[test]
    fn aggregate_delta_is_framed_validated_and_honors_the_negotiated_cap() {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let project = directory.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let now = Utc::now();
        write_rollout(&codex_home, &project, now);
        let config = CollectConfig {
            codex_home,
            rollout_cache_dir: Some(directory.path().join("cache")),
            ..CollectConfig::default()
        };
        let store =
            SourceIdentityStore::at_path(directory.path().join("state/source-identity.json"));
        let mut request = probe_request();
        request.expected_source = Some(source_generation(&store.load_or_create().unwrap()));
        request.max_page_bytes = 64 * 1024;
        request.request = RemoteExportRequestBody::Delta(DeltaRequest {
            delta_cursor: None,
            range: ExportRange {
                from: now - Duration::days(7),
                to: now,
            },
            overlap_minutes: 60,
            include_live: false,
            known_live_revision: None,
        });
        request.validate_remote_protocol().unwrap();

        let frame = serve_frame(&config, &store, &request);
        let encoded_len = u32::from_be_bytes(frame[12..16].try_into().unwrap()) as usize;
        assert!(encoded_len <= request.max_page_bytes as usize);
        let response = decode_remote_response_for_request::<DeltaPayload, EmptyRemotePayload>(
            &frame,
            &request,
            RemoteFrameLimits::default(),
        )
        .unwrap();
        let RemoteExportResponseBody::Delta { page, payload } = response.result else {
            panic!("expected aggregate delta response");
        };
        assert!(page.through_sequence > 0);
        assert!(!payload.bucket_changes.is_empty() || !payload.session_digest_changes.is_empty());
        assert_eq!(
            payload.stats.journal_records_scanned,
            payload.bucket_changes.len() as u64 + payload.session_digest_changes.len() as u64
        );
    }

    #[test]
    fn session_fact_snapshot_is_framed_typed_and_content_free() {
        let directory = tempfile::tempdir().unwrap();
        let codex_home = directory.path().join("codex");
        let project = directory.path().join("private-project-name");
        fs::create_dir_all(&project).unwrap();
        let now = Utc::now();
        write_rollout(&codex_home, &project, now);
        let config = CollectConfig {
            codex_home,
            rollout_cache_dir: Some(directory.path().join("cache")),
            ..CollectConfig::default()
        };
        let store =
            SourceIdentityStore::at_path(directory.path().join("state/source-identity.json"));
        let identity = store.load_or_create().unwrap();
        let mut request = probe_request();
        request.expected_source = Some(source_generation(&identity));
        let thread_id = "01a00000-0000-7000-8000-000000000001".parse().unwrap();
        let expected_digests = expected_fact_digests(&config, &identity, &thread_id, now);
        request.request =
            RemoteExportRequestBody::SessionFacts(crate::remote_protocol::SessionFactsRequest {
                thread_id,
                retention_days: 35,
                expected_digests,
                position: crate::remote_protocol::SessionFactsPosition::SnapshotStart,
            });
        request.validate_remote_protocol().unwrap();

        let frame = serve_frame(&config, &store, &request);
        assert!(frame.len() <= request.max_page_bytes as usize + 20);
        let response =
            decode_remote_response_for_request::<DeltaPayload, RemoteSessionFactPayload>(
                &frame,
                &request,
                RemoteFrameLimits::default(),
            )
            .unwrap();
        let RemoteExportResponseBody::FactSnapshot { page, payload } = response.result else {
            panic!("expected fact snapshot response");
        };
        assert!(!page.has_more);
        assert!(page.activate_fact_cursor.is_some());
        let RemoteSessionFactPayload::Snapshot(payload) = payload else {
            panic!("expected typed snapshot fact payload");
        };
        assert_eq!(payload.records.len(), 1);
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("private-project-name"));
        assert!(!serialized.contains(&project.to_string_lossy().into_owned()));
        assert!(serialized.contains("opk-hmac-sha256-v1-"));
    }
}
