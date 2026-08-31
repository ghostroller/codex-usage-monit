//! Versioned, side-effect-free wire types for the short-lived remote exporter.
//!
//! This module deliberately contains no SSH, rollout discovery, persistence,
//! or command-line wiring. It defines the protocol boundary and a single-frame
//! JSON codec that can be used over stdin/stdout by a later runtime layer.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{Cursor, Read, Write};
use std::num::{NonZeroU32, NonZeroU64};
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use flate2::Compression;
use flate2::bufread::GzDecoder;
use flate2::write::GzEncoder;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::{TaskStatus, TurnStatus};
use crate::source_history::{RedactionProfile, UsageEventId};
use crate::source_identity::NodeId;
use crate::source_model::{ObservedProjectKey, ProjectDisplayLabel, ThreadId};

/// Second remote export schema: project Git probe results are explicit and
/// live snapshot limits are enforced symmetrically at both trust boundaries.
pub const REMOTE_PROTOCOL_VERSION: u32 = 2;
/// First normalized per-event fact schema. It is intentionally independent
/// from aggregate-history revisions so a center can reject an unknown fact
/// shape without discarding otherwise compatible bucket data.
pub const REMOTE_SESSION_FACT_SCHEMA_VERSION: u32 = 3;

/// Maximum accepted compressed (or identity) JSON payload size.
pub const MAX_REMOTE_FRAME_ENCODED_BYTES: usize = 4 * 1024 * 1024;
/// Smallest negotiated response payload cap. This leaves enough room for a
/// bounded structured failure after a request has been decoded, instead of
/// turning a too-small page preference into an ambiguous empty SSH response.
pub const MIN_REMOTE_RESPONSE_ENCODED_BYTES: usize = 4 * 1024;
/// Maximum accepted JSON size after decompression.
pub const MAX_REMOTE_FRAME_DECODED_BYTES: usize = 32 * 1024 * 1024;
/// Encoder threshold below which identity framing is preferred.
pub const REMOTE_IDENTITY_THRESHOLD_BYTES: usize = 32 * 1024;

const FRAME_MAGIC: [u8; 8] = *b"CUMRMT01";
const FRAME_VERSION: u8 = 1;
const FRAME_HEADER_BYTES: usize = 20;
const ENCODING_IDENTITY: u8 = 0;
const ENCODING_GZIP: u8 = 1;
const MAX_VERSION_BYTES: usize = 96;
const MAX_OPAQUE_TOKEN_BYTES: usize = 1024;
const MAX_ERROR_MESSAGE_CHARS: usize = 512;
const MAX_EXPORT_RANGE_DAYS: i64 = 35;
const MAX_OVERLAP_MINUTES: u16 = 24 * 60;
const MAX_REMOTE_OPERATION_DURATION: Duration = Duration::hours(24);
const SOURCE_BUCKET_MINUTES: i64 = 15;
const MAX_PROJECT_DESCRIPTORS_PER_PAGE: usize = 16_384;
const MAX_BUCKET_CHANGES_PER_PAGE: usize = 32_768;
const MAX_SESSION_DIGEST_CHANGES_PER_PAGE: usize = 32_768;
const MAX_SESSION_FACT_RECORDS_PER_PAGE: usize = 32_768;
const MAX_SESSION_FACT_DIGEST_BINDINGS: usize = 36;
const MAX_BUCKET_GROUPS: usize = 8_192;
const MAX_BUCKET_PROJECT_GROUPS: usize = 16_384;
/// Source-side live snapshots are deliberately much smaller than aggregate
/// history pages. These constants are shared by the exporter and the center
/// validator so a malicious peer cannot bypass the source's wire budget.
pub(crate) const MAX_LIVE_TASKS: usize = 128;
pub(crate) const MAX_LIVE_TURNS: usize = 512;
pub(crate) const MAX_LIVE_SERIALIZED_BYTES: usize = 64 * 1024;
const MAX_PARTIAL_REASONS: usize = 128;
const MAX_WARNINGS_PER_PAGE: usize = 128;
const MAX_PARTIAL_REASON_BYTES: usize = 160;
const MAX_WARNING_CODE_BYTES: usize = 96;
const MAX_MODEL_BYTES: usize = 256;
const MAX_SERVICE_TIER_BYTES: usize = 64;
const MAX_REASONING_EFFORT_BYTES: usize = 64;
const MAX_TURN_ID_BYTES: usize = 256;
const MAX_PREVIEW_BYTES: usize = 4 * 1024;
const MAX_PREVIEW_CHARS: usize = 1_024;
const MAX_REPOSITORY_RELATIVE_ROOT_BYTES: usize = 2 * 1024;
const GIT_FINGERPRINT_PREFIX: &str = "git-sha256-v1-";
const SESSION_DIGEST_FINGERPRINT_PREFIX: &str = "session-digest-sha256-v1-";
const SHA256_HEX_BYTES: usize = 64;

/// Limits applied before allocation and throughout frame decoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteFrameLimits {
    pub max_encoded_bytes: usize,
    pub max_decoded_bytes: usize,
    pub identity_threshold_bytes: usize,
}

impl Default for RemoteFrameLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: MAX_REMOTE_FRAME_ENCODED_BYTES,
            max_decoded_bytes: MAX_REMOTE_FRAME_DECODED_BYTES,
            identity_threshold_bytes: REMOTE_IDENTITY_THRESHOLD_BYTES,
        }
    }
}

impl RemoteFrameLimits {
    fn validate(self) -> Result<(), RemoteProtocolError> {
        if self.max_encoded_bytes == 0
            || self.max_decoded_bytes == 0
            || self.max_encoded_bytes > u32::MAX as usize
            || self.max_decoded_bytes > u32::MAX as usize
            || self.identity_threshold_bytes > self.max_decoded_bytes
        {
            return Err(RemoteProtocolError::new(
                RemoteProtocolErrorKind::InvalidLimits,
                "remote frame limits are invalid",
            ));
        }
        Ok(())
    }
}

/// Stable category for callers that need to report or retry protocol errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteProtocolErrorKind {
    InvalidLimits,
    InvalidMessage,
    InvalidMagic,
    UnsupportedFrameVersion,
    UnsupportedEncoding,
    InvalidHeader,
    EncodedLimitExceeded,
    DecodedLimitExceeded,
    TruncatedFrame,
    TrailingFrameData,
    LengthMismatch,
    Compression,
    TrailingCompressedData,
    InvalidJson,
}

/// A bounded protocol/codec failure. It never embeds decoded remote content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteProtocolError {
    kind: RemoteProtocolErrorKind,
    message: String,
}

impl RemoteProtocolError {
    fn new(kind: RemoteProtocolErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> RemoteProtocolErrorKind {
        self.kind
    }
}

impl fmt::Display for RemoteProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteProtocolError {}

/// Semantic validation required before a message enters or leaves the frame.
pub trait RemoteProtocolMessage {
    fn validate_remote_protocol(&self) -> Result<(), RemoteProtocolError>;
}

/// Validation hook for page payloads owned by later exporter/storage layers.
pub trait RemotePagePayload {
    fn validate_remote_payload(&self) -> Result<(), RemoteProtocolError>;

    /// Optional aggregate-delta validation hook. Fact payloads and the strict
    /// empty placeholder use the default no-op; [`DeltaPayload`] binds its
    /// contents to the response envelope, page, and (when available) request.
    fn validate_remote_delta_payload(
        &self,
        _context: &RemoteDeltaPayloadContext<'_>,
    ) -> Result<(), RemoteProtocolError> {
        Ok(())
    }

    /// Optional session-fact validation hook. Aggregate payloads and the
    /// strict empty placeholder use the default no-op; [`RemoteSessionFactPayload`]
    /// binds every fact/change to its page, revision set, and request cursor.
    fn validate_remote_fact_payload(
        &self,
        _context: &RemoteFactPayloadContext<'_>,
    ) -> Result<(), RemoteProtocolError> {
        Ok(())
    }
}

/// Strict empty payload useful for probes and protocol-only tests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyRemotePayload {}

impl RemotePagePayload for EmptyRemotePayload {
    fn validate_remote_payload(&self) -> Result<(), RemoteProtocolError> {
        Ok(())
    }
}

/// Encodes one complete JSON message into one stdout-safe frame.
///
/// The JSON size determines identity versus fast gzip. Both encoded and
/// decoded lengths are authenticated by the surrounding transport contract;
/// gzip additionally verifies CRC and its own uncompressed length.
pub fn encode_remote_frame<T>(
    message: &T,
    limits: RemoteFrameLimits,
) -> Result<Vec<u8>, RemoteProtocolError>
where
    T: Serialize + RemoteProtocolMessage,
{
    limits.validate()?;
    message.validate_remote_protocol()?;
    let mut json = LimitedJsonBuffer::new(limits.max_decoded_bytes);
    if let Err(error) = serde_json::to_writer(&mut json, message) {
        return Err(if json.exceeded {
            RemoteProtocolError::new(
                RemoteProtocolErrorKind::DecodedLimitExceeded,
                "remote JSON exceeds the decoded frame limit",
            )
        } else {
            RemoteProtocolError::new(
                RemoteProtocolErrorKind::InvalidJson,
                format!("could not serialize remote protocol message: {error}"),
            )
        });
    }
    let json = json.bytes;

    let decoded_len = json.len();
    let (encoding, payload) = if decoded_len <= limits.identity_threshold_bytes {
        (ENCODING_IDENTITY, json)
    } else {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&json).map_err(compression_error)?;
        let payload = encoder.finish().map_err(compression_error)?;
        (ENCODING_GZIP, payload)
    };
    if payload.len() > limits.max_encoded_bytes {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::EncodedLimitExceeded,
            "remote JSON exceeds the encoded frame limit",
        ));
    }

    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.push(FRAME_VERSION);
    frame.push(encoding);
    frame.extend_from_slice(&0_u16.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&(decoded_len as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

struct LimitedJsonBuffer {
    bytes: Vec<u8>,
    maximum: usize,
    exceeded: bool,
}

impl LimitedJsonBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(maximum.min(64 * 1024)),
            maximum,
            exceeded: false,
        }
    }
}

impl Write for LimitedJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::other("remote JSON size overflow"));
        };
        if next_len > self.maximum {
            self.exceeded = true;
            return Err(std::io::Error::other("remote JSON exceeds its limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Writes exactly one encoded frame without touching stderr or flushing the
/// caller-owned stream.
pub fn write_remote_frame<T, W>(
    writer: &mut W,
    message: &T,
    limits: RemoteFrameLimits,
) -> Result<(), RemoteProtocolError>
where
    T: Serialize + RemoteProtocolMessage,
    W: Write,
{
    let frame = encode_remote_frame(message, limits)?;
    writer.write_all(&frame).map_err(|error| {
        RemoteProtocolError::new(
            RemoteProtocolErrorKind::TruncatedFrame,
            format!("could not write complete remote frame: {error}"),
        )
    })
}

/// Reads a single frame from a stream into bounded memory.
///
/// At most the header, the configured encoded maximum, and one tail-detection
/// byte are consumed. Process timeouts remain the transport layer's concern.
pub fn read_remote_frame<T, R>(
    reader: R,
    limits: RemoteFrameLimits,
) -> Result<T, RemoteProtocolError>
where
    T: DeserializeOwned + RemoteProtocolMessage,
    R: Read,
{
    limits.validate()?;
    let read_limit = FRAME_HEADER_BYTES
        .checked_add(limits.max_encoded_bytes)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| {
            RemoteProtocolError::new(
                RemoteProtocolErrorKind::EncodedLimitExceeded,
                "remote stream read limit overflows this platform",
            )
        })?;
    let mut frame = Vec::with_capacity(read_limit.min(64 * 1024));
    reader
        .take(read_limit as u64)
        .read_to_end(&mut frame)
        .map_err(|error| {
            RemoteProtocolError::new(
                RemoteProtocolErrorKind::TruncatedFrame,
                format!("could not read complete remote frame: {error}"),
            )
        })?;
    decode_remote_frame(&frame, limits)
}

/// Decodes exactly one complete frame and rejects truncation, any frame tail,
/// gzip tails/concatenated members, bombs, CRC errors, and invalid schemas.
pub fn decode_remote_frame<T>(
    frame: &[u8],
    limits: RemoteFrameLimits,
) -> Result<T, RemoteProtocolError>
where
    T: DeserializeOwned + RemoteProtocolMessage,
{
    limits.validate()?;
    if frame.len() < FRAME_HEADER_BYTES {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::TruncatedFrame,
            "remote frame header is truncated",
        ));
    }
    if frame[..FRAME_MAGIC.len()] != FRAME_MAGIC {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::InvalidMagic,
            "remote frame magic is invalid",
        ));
    }
    if frame[8] != FRAME_VERSION {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::UnsupportedFrameVersion,
            "remote frame version is unsupported",
        ));
    }
    let encoding = frame[9];
    if !matches!(encoding, ENCODING_IDENTITY | ENCODING_GZIP) {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::UnsupportedEncoding,
            "remote frame encoding is unsupported",
        ));
    }
    if frame[10..12] != [0, 0] {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::InvalidHeader,
            "remote frame reserved flags must be zero",
        ));
    }

    let encoded_len = read_u32(&frame[12..16]) as usize;
    let decoded_len = read_u32(&frame[16..20]) as usize;
    if encoded_len > limits.max_encoded_bytes {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::EncodedLimitExceeded,
            "remote frame declares an encoded payload above the limit",
        ));
    }
    if decoded_len > limits.max_decoded_bytes {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::DecodedLimitExceeded,
            "remote frame declares a decoded payload above the limit",
        ));
    }
    let expected_frame_len = FRAME_HEADER_BYTES.checked_add(encoded_len).ok_or_else(|| {
        RemoteProtocolError::new(
            RemoteProtocolErrorKind::EncodedLimitExceeded,
            "remote frame length overflows this platform",
        )
    })?;
    if frame.len() < expected_frame_len {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::TruncatedFrame,
            "remote frame payload is truncated",
        ));
    }
    if frame.len() > expected_frame_len {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::TrailingFrameData,
            "remote frame has trailing data",
        ));
    }

    let payload = &frame[FRAME_HEADER_BYTES..];
    let json = match encoding {
        ENCODING_IDENTITY => {
            if encoded_len != decoded_len {
                return Err(RemoteProtocolError::new(
                    RemoteProtocolErrorKind::LengthMismatch,
                    "identity frame encoded and decoded lengths differ",
                ));
            }
            payload.to_vec()
        }
        ENCODING_GZIP => decode_gzip_exact(payload, decoded_len)?,
        _ => unreachable!("encoding was checked above"),
    };

    let message = serde_json::from_slice::<T>(&json).map_err(|error| {
        RemoteProtocolError::new(
            RemoteProtocolErrorKind::InvalidJson,
            format!(
                "remote JSON schema is invalid at line {} column {}",
                error.line(),
                error.column()
            ),
        )
    })?;
    message.validate_remote_protocol()?;
    Ok(message)
}

/// Returns the authenticated decoded JSON length from a frame that has
/// already passed [`decode_remote_frame`]. Multi-page consumers use this
/// exact value to enforce a run-level memory budget in addition to the
/// process-wide per-frame limit.
pub fn decoded_remote_frame_payload_len(frame: &[u8]) -> Result<usize, RemoteProtocolError> {
    if frame.len() < FRAME_HEADER_BYTES {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::TruncatedFrame,
            "remote frame header is truncated",
        ));
    }
    Ok(read_u32(&frame[16..20]) as usize)
}

/// Encodes a response while enforcing the request's negotiated compressed-page
/// cap in addition to the process-wide compressed and decoded limits.
pub fn encode_remote_response_for_request<D, F>(
    response: &RemoteExportResponse<D, F>,
    request: &RemoteExportRequest,
    limits: RemoteFrameLimits,
) -> Result<Vec<u8>, RemoteProtocolError>
where
    D: Serialize + RemotePagePayload,
    F: Serialize + RemotePagePayload,
{
    request.validate_remote_protocol()?;
    response.validate_for_request(request)?;
    encode_remote_frame(response, negotiated_response_limits(request, limits)?)
}

/// Decodes a response with the negotiated compressed-page cap applied before
/// payload allocation, then validates its source/profile/revision/cursor
/// relationship to the exact request.
pub fn decode_remote_response_for_request<D, F>(
    frame: &[u8],
    request: &RemoteExportRequest,
    limits: RemoteFrameLimits,
) -> Result<RemoteExportResponse<D, F>, RemoteProtocolError>
where
    D: DeserializeOwned + RemotePagePayload,
    F: DeserializeOwned + RemotePagePayload,
{
    request.validate_remote_protocol()?;
    let response: RemoteExportResponse<D, F> =
        decode_remote_frame(frame, negotiated_response_limits(request, limits)?)?;
    response.validate_for_request(request)?;
    Ok(response)
}

fn negotiated_response_limits(
    request: &RemoteExportRequest,
    mut limits: RemoteFrameLimits,
) -> Result<RemoteFrameLimits, RemoteProtocolError> {
    limits.validate()?;
    limits.max_encoded_bytes = limits
        .max_encoded_bytes
        .min(request.max_page_bytes as usize);
    limits.validate()?;
    Ok(limits)
}

fn decode_gzip_exact(
    payload: &[u8],
    expected_decoded_len: usize,
) -> Result<Vec<u8>, RemoteProtocolError> {
    let cursor = Cursor::new(payload);
    let mut decoder = GzDecoder::new(cursor);
    let read_limit = expected_decoded_len.checked_add(1).ok_or_else(|| {
        RemoteProtocolError::new(
            RemoteProtocolErrorKind::DecodedLimitExceeded,
            "remote gzip decoded length overflows this platform",
        )
    })?;
    let mut decoded = Vec::with_capacity(expected_decoded_len.min(64 * 1024));
    decoder
        .by_ref()
        .take(read_limit as u64)
        .read_to_end(&mut decoded)
        .map_err(compression_error)?;
    if decoded.len() != expected_decoded_len {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::LengthMismatch,
            "remote gzip decoded length does not match its frame header",
        ));
    }

    let mut extra_decoded = [0_u8; 1];
    if decoder
        .read(&mut extra_decoded)
        .map_err(compression_error)?
        != 0
    {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::LengthMismatch,
            "remote gzip expands beyond its declared decoded length",
        ));
    }
    let consumed = decoder.into_inner().position() as usize;
    if consumed != payload.len() {
        return Err(RemoteProtocolError::new(
            RemoteProtocolErrorKind::TrailingCompressedData,
            "remote gzip payload has trailing data or another member",
        ));
    }
    Ok(decoded)
}

fn compression_error(error: std::io::Error) -> RemoteProtocolError {
    RemoteProtocolError::new(
        RemoteProtocolErrorKind::Compression,
        format!("remote gzip payload is invalid: {error}"),
    )
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("a fixed four-byte header field"))
}

/// Bounded, terminal-safe application version exchanged during negotiation.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct BinaryVersion(String);

impl BinaryVersion {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BinaryVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for BinaryVersion {
    type Err = RemoteProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > MAX_VERSION_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+')
            })
        {
            return Err(RemoteProtocolError::new(
                RemoteProtocolErrorKind::InvalidMessage,
                "binary version contains invalid characters or length",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

impl Serialize for BinaryVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BinaryVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Public identity and generation pinned by a center for one source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceGeneration {
    pub node_id: NodeId,
    pub generation: NonZeroU64,
}

/// One concrete format/catalog revision selected by the exporter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolRevisions {
    pub history_format: NonZeroU32,
    pub metric: NonZeroU32,
    pub estimator: NonZeroU32,
    pub project_breakdown: NonZeroU32,
    pub api_pricing_catalog: NonZeroU32,
}

/// Inclusive accepted range for one independently versioned data domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcceptedRevisionRange {
    pub min: NonZeroU32,
    pub max: NonZeroU32,
}

impl AcceptedRevisionRange {
    fn validate(self) -> Result<(), RemoteProtocolError> {
        if self.min > self.max {
            return Err(invalid_message(
                "accepted revision minimum exceeds its maximum",
            ));
        }
        Ok(())
    }

    pub fn accepts(self, revision: NonZeroU32) -> bool {
        self.min <= revision && revision <= self.max
    }
}

/// Revisions the center can safely consume in each independent domain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcceptedRevisions {
    pub history_format: AcceptedRevisionRange,
    pub metric: AcceptedRevisionRange,
    pub estimator: AcceptedRevisionRange,
    pub project_breakdown: AcceptedRevisionRange,
    pub api_pricing_catalog: AcceptedRevisionRange,
}

impl AcceptedRevisions {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.history_format.validate()?;
        self.metric.validate()?;
        self.estimator.validate()?;
        self.project_breakdown.validate()?;
        self.api_pricing_catalog.validate()?;
        Ok(())
    }

    pub fn accepts(&self, revisions: &ProtocolRevisions) -> bool {
        self.history_format.accepts(revisions.history_format)
            && self.metric.accepts(revisions.metric)
            && self.estimator.accepts(revisions.estimator)
            && self.project_breakdown.accepts(revisions.project_breakdown)
            && self
                .api_pricing_catalog
                .accepts(revisions.api_pricing_catalog)
    }
}

/// Cursor for the aggregate bucket/session-digest journal only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeltaCursor {
    pub generation: NonZeroU64,
    pub sequence: u64,
}

/// Cursor for one source/thread/redaction event-fact set only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FactCursor {
    pub fact_generation: NonZeroU64,
    pub through_sequence: u64,
}

macro_rules! opaque_wire_type {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = RemoteProtocolError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if value.is_empty()
                    || value.len() > MAX_OPAQUE_TOKEN_BYTES
                    || !value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
                    })
                {
                    return Err(invalid_message(concat!($label, " is invalid")));
                }
                Ok(Self(value.to_owned()))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_wire_type!(FactSnapshotId, "fact snapshot ID");
opaque_wire_type!(FactBatchId, "fact batch ID");
opaque_wire_type!(FactSnapshotPageToken, "fact snapshot page token");
opaque_wire_type!(FactDeltaPageToken, "fact delta page token");

/// Strict request envelope. The adjacently tagged body makes request kinds
/// mutually exclusive: a probe can never also carry delta/session parameters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExportRequest {
    pub protocol_version: u32,
    pub client_version: BinaryVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_source: Option<SourceGeneration>,
    pub redaction_profile: RedactionProfile,
    pub max_page_bytes: u32,
    pub accepted_revisions: AcceptedRevisions,
    pub request: RemoteExportRequestBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "requestKind",
    content = "parameters",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteExportRequestBody {
    Probe(ProbeRequest),
    Delta(DeltaRequest),
    SessionFacts(SessionFactsRequest),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbeRequest {
    pub check_state_writable: bool,
    pub check_rollout_readable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl ExportRange {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        if self.from >= self.to {
            return Err(invalid_message("export range must have from before to"));
        }
        if self.to.signed_duration_since(self.from) > Duration::days(MAX_EXPORT_RANGE_DAYS) {
            return Err(invalid_message("export range exceeds retention window"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeltaRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_cursor: Option<DeltaCursor>,
    /// Collection-coverage target. This is not a delta-record filter: the
    /// source/profile journal has one global cursor, so every transition must
    /// be transferred before that cursor advances. Query/UI time filtering is
    /// performed by the center after durable ingest.
    pub range: ExportRange,
    pub overlap_minutes: u16,
    pub include_live: bool,
    /// Last live replacement durably retained by the center. A mismatched or
    /// absent value requires the exporter to resend the full snapshot even
    /// when its semantic revision has not changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_live_revision: Option<NonZeroU64>,
}

/// Fact pagination mode. Snapshot and delta page tokens are separate Rust and
/// wire types, and neither can be substituted for a [`DeltaCursor`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "positionKind",
    content = "position",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum SessionFactsPosition {
    SnapshotStart,
    SnapshotContinue {
        snapshot_id: FactSnapshotId,
        fact_generation: NonZeroU64,
        snapshot_watermark: u64,
        page_token: FactSnapshotPageToken,
    },
    DeltaStart {
        fact_cursor: FactCursor,
    },
    DeltaContinue {
        fact_cursor: FactCursor,
        batch_id: FactBatchId,
        delta_watermark: u64,
        page_token: FactDeltaPageToken,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionFactsRequest {
    pub thread_id: ThreadId,
    pub retention_days: u16,
    /// Exact UTC-day known-event digests observed by the center when this
    /// refresh was planned. Coverage may remain an explicit lower bound. The
    /// exporter must reproduce every binding from the same complete rollout
    /// scan that materializes the returned facts.
    pub expected_digests: Vec<SessionFactsDigestBinding>,
    pub position: SessionFactsPosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionFactsDigestBinding {
    pub range_start: DateTime<Utc>,
    pub range_end: DateTime<Utc>,
    pub covered_through: DateTime<Utc>,
    pub coverage_complete: bool,
    pub fingerprint: RemoteSessionDigestFingerprint,
    pub project_breakdown_fingerprint: RemoteSessionDigestFingerprint,
    pub event_count: u64,
    pub metric_revision: NonZeroU32,
    pub estimator_revision: NonZeroU32,
    pub project_breakdown_revision: NonZeroU32,
    pub api_pricing_catalog_revision: NonZeroU32,
}

impl SessionFactsDigestBinding {
    fn validate(&self, accepted_revisions: &AcceptedRevisions) -> Result<(), RemoteProtocolError> {
        if self.range_end <= self.range_start
            || self.range_end.signed_duration_since(self.range_start) > Duration::days(1)
            || self.covered_through < self.range_start
            || self.covered_through > self.range_end
            || (self.coverage_complete && self.covered_through != self.range_end)
        {
            return Err(invalid_message(
                "session-fact digest binding range is invalid",
            ));
        }
        if !accepted_revisions.metric.accepts(self.metric_revision)
            || !accepted_revisions
                .estimator
                .accepts(self.estimator_revision)
            || !accepted_revisions
                .project_breakdown
                .accepts(self.project_breakdown_revision)
            || !accepted_revisions
                .api_pricing_catalog
                .accepts(self.api_pricing_catalog_revision)
        {
            return Err(invalid_message(
                "session-fact digest binding revisions are unsupported",
            ));
        }
        Ok(())
    }
}

impl RemoteProtocolMessage for RemoteExportRequest {
    fn validate_remote_protocol(&self) -> Result<(), RemoteProtocolError> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(invalid_message(
                "remote request protocol version is unsupported",
            ));
        }
        if self.max_page_bytes as usize > MAX_REMOTE_FRAME_ENCODED_BYTES
            || (self.max_page_bytes as usize) < MIN_REMOTE_RESPONSE_ENCODED_BYTES
        {
            return Err(invalid_message("remote request maxPageBytes is invalid"));
        }
        self.accepted_revisions.validate()?;
        match &self.request {
            RemoteExportRequestBody::Probe(_) => Ok(()),
            RemoteExportRequestBody::Delta(request) => {
                if self.expected_source.is_none() {
                    return Err(invalid_message(
                        "delta requests require a pinned source identity",
                    ));
                }
                request.range.validate()?;
                if request.overlap_minutes > MAX_OVERLAP_MINUTES {
                    return Err(invalid_message("delta overlapMinutes is too large"));
                }
                if !request.include_live && request.known_live_revision.is_some() {
                    return Err(invalid_message("knownLiveRevision requires includeLive"));
                }
                Ok(())
            }
            RemoteExportRequestBody::SessionFacts(request) => {
                if self.expected_source.is_none() {
                    return Err(invalid_message(
                        "session-fact requests require a pinned source identity",
                    ));
                }
                if request.retention_days == 0
                    || i64::from(request.retention_days) > MAX_EXPORT_RANGE_DAYS
                {
                    return Err(invalid_message(
                        "session-fact retentionDays is outside the supported window",
                    ));
                }
                if request.expected_digests.is_empty()
                    || request.expected_digests.len() > MAX_SESSION_FACT_DIGEST_BINDINGS
                {
                    return Err(invalid_message(
                        "session-fact digest binding count is invalid",
                    ));
                }
                let mut previous = None;
                for binding in &request.expected_digests {
                    binding.validate(&self.accepted_revisions)?;
                    if previous.is_some_and(|start| start >= binding.range_start) {
                        return Err(invalid_message(
                            "session-fact digest bindings must be sorted and unique",
                        ));
                    }
                    previous = Some(binding.range_start);
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCapability {
    DeltaJournal,
    LiveSnapshot,
    SessionFactSnapshot,
    SessionFactDelta,
    RedactedContent,
    PreviewContent,
    GzipFrame,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbeResult {
    pub capabilities: Vec<RemoteCapability>,
    pub state_writable: bool,
    pub rollout_readable: bool,
}

impl ProbeResult {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        let unique = self.capabilities.iter().copied().collect::<BTreeSet<_>>();
        if unique.len() != self.capabilities.len() {
            return Err(invalid_message("probe capabilities contain duplicates"));
        }
        Ok(())
    }
}

/// SHA-256 fingerprint of a normalized Git remote identity. Raw repository
/// URLs, credentials, queries, and userinfo never cross this boundary.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitRepositoryFingerprint(String);

impl GitRepositoryFingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GitRepositoryFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for GitRepositoryFingerprint {
    type Err = RemoteProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_prefixed_lower_hex(
            value,
            GIT_FINGERPRINT_PREFIX,
            SHA256_HEX_BYTES,
            "Git repository fingerprint",
        )?;
        Ok(Self(value.to_owned()))
    }
}

impl Serialize for GitRepositoryFingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for GitRepositoryFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Content-free SHA-256 fingerprint for one normalized session range.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemoteSessionDigestFingerprint(String);

impl RemoteSessionDigestFingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteSessionDigestFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RemoteSessionDigestFingerprint {
    type Err = RemoteProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_prefixed_lower_hex(
            value,
            SESSION_DIGEST_FINGERPRINT_PREFIX,
            SHA256_HEX_BYTES,
            "session digest fingerprint",
        )?;
        Ok(Self(value.to_owned()))
    }
}

impl Serialize for RemoteSessionDigestFingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RemoteSessionDigestFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Exact unsigned 128-bit value represented canonically as a decimal JSON
/// string so non-Rust centers cannot lose EST precision.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemoteU128(u128);

impl RemoteU128 {
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u128 {
        self.0
    }
}

impl Serialize for RemoteU128 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for RemoteU128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty()
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(serde::de::Error::custom(
                "remote 128-bit integer is not canonical decimal",
            ));
        }
        value
            .parse::<u128>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Strict token breakdown used by aggregate delta records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteTokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

impl RemoteTokenUsage {
    fn validate(self) -> Result<(), RemoteProtocolError> {
        if self.cached_input_tokens > self.input_tokens
            || self.cache_write_input_tokens > self.input_tokens
            || self.reasoning_output_tokens > self.output_tokens
        {
            return Err(invalid_message(
                "remote token detail exceeds its containing input or output total",
            ));
        }
        let breakdown_total = self
            .input_tokens
            .checked_add(self.output_tokens)
            .ok_or_else(|| invalid_message("remote token breakdown overflows"))?;
        if breakdown_total != 0 && self.total_tokens != breakdown_total {
            return Err(invalid_message(
                "remote token total does not match its input/output breakdown",
            ));
        }
        if self.total_tokens == 0
            && (self.input_tokens != 0
                || self.cached_input_tokens != 0
                || self.cache_write_input_tokens != 0
                || self.output_tokens != 0
                || self.reasoning_output_tokens != 0)
        {
            return Err(invalid_message(
                "zero remote token total contains a nonzero component",
            ));
        }
        Ok(())
    }

    fn is_zero(self) -> bool {
        self.total_tokens == 0
            && self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_write_input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_output_tokens == 0
    }
}

/// Strict token-only API-equivalent range and its pricing coverage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteApiCostAmount {
    pub minimum_pico_usd: RemoteU128,
    pub maximum_pico_usd: RemoteU128,
    pub observed_samples: u64,
    pub priced_samples: u64,
    /// API-priced tokens are an independent population from Codex
    /// Tokens/EST. In particular, Spark calls are excluded from the latter
    /// while remaining valid API-equivalent model usage.
    pub observed_tokens: u64,
    pub priced_tokens: u64,
}

impl RemoteApiCostAmount {
    fn validate(self, _token_usage: RemoteTokenUsage) -> Result<(), RemoteProtocolError> {
        if self.minimum_pico_usd > self.maximum_pico_usd
            || self.priced_samples > self.observed_samples
            || self.priced_tokens > self.observed_tokens
        {
            return Err(invalid_message(
                "remote API-equivalent cost coverage is invalid",
            ));
        }
        if self.priced_samples == 0
            && (self.minimum_pico_usd.value() != 0 || self.maximum_pico_usd.value() != 0)
        {
            return Err(invalid_message(
                "unpriced remote usage cannot contain API-equivalent cost",
            ));
        }
        Ok(())
    }
}

/// Explicit result of the source's bounded Git probe. Keeping unavailability
/// distinct from an authoritative negative result lets the center preserve
/// verified evidence across transient failures while still clearing stale
/// merge suggestions when a workspace is no longer a repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteGitRepositoryEvidence {
    #[default]
    Unavailable,
    ConfirmedNonRepository,
    Repository {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<GitRepositoryFingerprint>,
        repository_relative_workspace_root: String,
    },
}

impl RemoteGitRepositoryEvidence {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        if let Self::Repository {
            repository_relative_workspace_root,
            ..
        } = self
        {
            validate_repository_relative_root(repository_relative_workspace_root)?;
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> Option<&GitRepositoryFingerprint> {
        match self {
            Self::Repository { fingerprint, .. } => fingerprint.as_ref(),
            Self::Unavailable | Self::ConfirmedNonRepository => None,
        }
    }

    pub fn repository_relative_workspace_root(&self) -> Option<&str> {
        match self {
            Self::Repository {
                repository_relative_workspace_root,
                ..
            } => Some(repository_relative_workspace_root),
            Self::Unavailable | Self::ConfirmedNonRepository => None,
        }
    }
}

/// Sanitized, source-owned project metadata. This deliberately has no absolute
/// path or repository URL field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteProjectDescriptor {
    pub observed_project_key: ObservedProjectKey,
    pub display_label: ProjectDisplayLabel,
    pub git_evidence: RemoteGitRepositoryEvidence,
}

impl RemoteProjectDescriptor {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.git_evidence.validate()
    }

    pub(crate) fn validate_for_storage(&self) -> Result<(), RemoteProtocolError> {
        self.validate()
    }
}

/// Coverage carried with every page so a bootstrap subset can never be
/// mistaken for a complete requested range. It qualifies collection evidence
/// and deliberately does not restrict the global journal transitions carried
/// by the page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteDeltaCoverage {
    pub requested_range: ExportRange,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_range: Option<ExportRange>,
    pub range_complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
}

impl RemoteDeltaCoverage {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        self.requested_range.validate()?;
        validate_partial_reasons(&self.partial_reasons, "delta coverage")?;
        if let Some(covered) = &self.covered_range {
            covered.validate()?;
            if covered.from < self.requested_range.from || covered.to > self.requested_range.to {
                return Err(invalid_message(
                    "remote covered range lies outside its requested range",
                ));
            }
        }
        if self.range_complete {
            if self.covered_range.as_ref() != Some(&self.requested_range) {
                return Err(invalid_message(
                    "range-complete remote coverage must exactly cover the request",
                ));
            }
        } else if self.partial_reasons.is_empty() {
            return Err(invalid_message(
                "partial remote coverage requires a machine-readable reason",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeltaPage {
    pub generation: NonZeroU64,
    pub from_sequence: u64,
    pub through_sequence: u64,
    pub next_delta_cursor: DeltaCursor,
    pub has_more: bool,
}

impl DeltaPage {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        if self.from_sequence > self.through_sequence {
            return Err(invalid_message("delta page sequence range is reversed"));
        }
        if (self.through_sequence > 0 && self.from_sequence == 0)
            || (self.through_sequence == 0 && self.has_more)
        {
            return Err(invalid_message(
                "delta page zero-sequence boundary is invalid",
            ));
        }
        if self.next_delta_cursor.generation != self.generation
            || self.next_delta_cursor.sequence != self.through_sequence
        {
            return Err(invalid_message(
                "delta page next cursor does not match its watermark",
            ));
        }
        Ok(())
    }
}

/// Model/service-tier aggregate within one source bucket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteModelUsageGroup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub token_usage: RemoteTokenUsage,
    pub estimated_cost_units: RemoteU128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<RemoteU128>,
    pub api_equivalent_cost: RemoteApiCostAmount,
    pub call_count: u64,
    pub used_model_fallback: bool,
    pub used_token_breakdown_fallback: bool,
    pub used_long_context_pricing: bool,
    pub used_long_context_detection_fallback: bool,
}

impl RemoteModelUsageGroup {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        validate_optional_protocol_text(self.model.as_deref(), MAX_MODEL_BYTES, "model")?;
        validate_optional_protocol_text(
            self.service_tier.as_deref(),
            MAX_SERVICE_TIER_BYTES,
            "service tier",
        )?;
        self.token_usage.validate()?;
        self.api_equivalent_cost.validate(self.token_usage)?;
        validate_usage_count(self.call_count, self.token_usage, "model group")
    }

    fn sort_key(&self) -> (Option<&str>, Option<&str>) {
        (self.model.as_deref(), self.service_tier.as_deref())
    }
}

/// One emitting thread/turn's own additive usage and exact root attribution.
/// Parent/subtree totals are intentionally absent from the wire schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteProjectUsageGroup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_project_key: Option<ObservedProjectKey>,
    pub emitting_thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitting_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_session_thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_session_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_preview: Option<String>,
    pub token_usage: RemoteTokenUsage,
    pub estimated_cost_units: RemoteU128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<RemoteU128>,
    pub api_equivalent_cost: RemoteApiCostAmount,
    pub call_count: u64,
}

impl RemoteProjectUsageGroup {
    fn validate(&self, profile: RedactionProfile) -> Result<(), RemoteProtocolError> {
        validate_optional_protocol_text(
            self.emitting_turn_id.as_deref(),
            MAX_TURN_ID_BYTES,
            "emitting turn ID",
        )?;
        validate_optional_protocol_text(
            self.root_session_turn_id.as_deref(),
            MAX_TURN_ID_BYTES,
            "root session turn ID",
        )?;
        if self.root_session_turn_id.is_some() && self.root_session_thread_id.is_none() {
            return Err(invalid_message(
                "remote root turn requires a root session thread",
            ));
        }
        validate_preview(self.title_preview.as_deref(), profile, "title preview")?;
        validate_preview(self.message_preview.as_deref(), profile, "message preview")?;
        self.token_usage.validate()?;
        self.api_equivalent_cost.validate(self.token_usage)?;
        validate_usage_count(self.call_count, self.token_usage, "project group")
    }

    fn sort_key(&self) -> RemoteProjectUsageGroupSortKey<'_> {
        RemoteProjectUsageGroupSortKey {
            observed_project_key: self
                .observed_project_key
                .as_ref()
                .map(ObservedProjectKey::as_str),
            emitting_thread_id: self.emitting_thread_id.as_str(),
            emitting_turn_id: self.emitting_turn_id.as_deref(),
            parent_thread_id: self.parent_thread_id.as_ref().map(ThreadId::as_str),
            root_session_thread_id: self.root_session_thread_id.as_ref().map(ThreadId::as_str),
            root_session_turn_id: self.root_session_turn_id.as_deref(),
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct RemoteProjectUsageGroupSortKey<'a> {
    observed_project_key: Option<&'a str>,
    emitting_thread_id: &'a str,
    emitting_turn_id: Option<&'a str>,
    parent_thread_id: Option<&'a str>,
    root_session_thread_id: Option<&'a str>,
    root_session_turn_id: Option<&'a str>,
}

/// Strict 15-minute aggregate bucket. All fields are source-owned facts; the
/// center derives subtree/project totals at query time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteUsageBucket {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub sampled_at: DateTime<Utc>,
    pub token_usage: RemoteTokenUsage,
    pub estimated_cost_units: RemoteU128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<RemoteU128>,
    pub long_context_usage_unknown: bool,
    pub api_equivalent_cost: RemoteApiCostAmount,
    pub call_count: u64,
    pub metric_revision: NonZeroU32,
    pub estimator_revision: NonZeroU32,
    pub project_breakdown_revision: NonZeroU32,
    pub api_pricing_catalog_revision: NonZeroU32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_groups: Vec<RemoteModelUsageGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_groups: Vec<RemoteProjectUsageGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
}

impl RemoteUsageBucket {
    fn validate(&self, context: &RemoteDeltaPayloadContext<'_>) -> Result<(), RemoteProtocolError> {
        validate_bucket_bounds(self.starts_at, self.ends_at)?;
        if self.sampled_at < self.starts_at || self.sampled_at > context.observed_at {
            return Err(invalid_message(
                "remote bucket sample time is outside its observable range",
            ));
        }
        self.token_usage.validate()?;
        self.api_equivalent_cost.validate(self.token_usage)?;
        validate_usage_count(self.call_count, self.token_usage, "bucket")?;
        validate_revisions(
            self.metric_revision,
            self.estimator_revision,
            self.project_breakdown_revision,
            self.api_pricing_catalog_revision,
            context.revisions,
            "bucket",
        )?;
        validate_partial_reasons(&self.partial_reasons, "bucket")?;
        validate_count(
            self.model_groups.len(),
            MAX_BUCKET_GROUPS,
            "remote bucket model groups",
        )?;
        validate_count(
            self.project_groups.len(),
            MAX_BUCKET_PROJECT_GROUPS,
            "remote bucket project groups",
        )?;
        for group in &self.model_groups {
            group.validate()?;
        }
        if self
            .model_groups
            .windows(2)
            .any(|groups| groups[0].sort_key() >= groups[1].sort_key())
        {
            return Err(invalid_message(
                "remote bucket model groups must be sorted and unique",
            ));
        }
        for group in &self.project_groups {
            group.validate(context.redaction_profile)?;
        }
        if self
            .project_groups
            .windows(2)
            .any(|groups| groups[0].sort_key() >= groups[1].sort_key())
        {
            return Err(invalid_message(
                "remote bucket project groups must be sorted and unique",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "changeKind",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteUsageBucketMutation {
    Upsert(Box<RemoteUsageBucket>),
    Tombstone,
}

/// One journaled revision of a stable `(source, profile, startsAt)` bucket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteUsageBucketChange {
    pub sequence: NonZeroU64,
    pub starts_at: DateTime<Utc>,
    pub revision: NonZeroU64,
    pub mutation: RemoteUsageBucketMutation,
}

impl RemoteUsageBucketChange {
    fn validate(&self, context: &RemoteDeltaPayloadContext<'_>) -> Result<(), RemoteProtocolError> {
        validate_aligned_bucket_start(self.starts_at)?;
        if let RemoteUsageBucketMutation::Upsert(bucket) = &self.mutation {
            bucket.validate(context)?;
            if bucket.starts_at != self.starts_at {
                return Err(invalid_message(
                    "remote bucket change key does not match its payload",
                ));
            }
        }
        validate_change_sequence(self.sequence, context.page)
    }
}

/// Additive aggregate retained in both session digests and later fact pages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteSessionUsageMetrics {
    pub token_usage: RemoteTokenUsage,
    pub estimated_cost_units: RemoteU128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_long_context_extra_cost_units: Option<RemoteU128>,
    pub api_equivalent_cost: RemoteApiCostAmount,
    pub call_count: u64,
    pub metric_revision: NonZeroU32,
    pub estimator_revision: NonZeroU32,
    pub project_breakdown_revision: NonZeroU32,
    pub api_pricing_catalog_revision: NonZeroU32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial_reasons: Vec<String>,
}

impl RemoteSessionUsageMetrics {
    fn validate(&self, context: &RemoteDeltaPayloadContext<'_>) -> Result<(), RemoteProtocolError> {
        self.validate_for_revisions(context.revisions, "session digest")
    }

    fn validate_for_revisions(
        &self,
        revisions: &ProtocolRevisions,
        subject: &str,
    ) -> Result<(), RemoteProtocolError> {
        self.token_usage.validate()?;
        self.api_equivalent_cost.validate(self.token_usage)?;
        validate_usage_count(self.call_count, self.token_usage, subject)?;
        validate_revisions(
            self.metric_revision,
            self.estimator_revision,
            self.project_breakdown_revision,
            self.api_pricing_catalog_revision,
            revisions,
            subject,
        )?;
        validate_partial_reasons(&self.partial_reasons, subject)
    }
}

/// One normalized source-local usage event. The schema deliberately has no
/// prompt, assistant message, reasoning text, tool body, title, cwd, or raw
/// repository field. Project identity is the source-private HMAC key already
/// used by aggregate pages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteUsageEventFact {
    pub event_id: UsageEventId,
    pub occurred_at: DateTime<Utc>,
    pub observed_project_key: ObservedProjectKey,
    pub emitting_thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitting_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    /// Exact optional sessionThreadId stored in the source project group.
    /// This remains distinct from the required root-session query fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_session_thread_id: Option<ThreadId>,
    pub root_session_thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_session_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Exact source event tokens committed into the canonical session digest.
    /// Accounting metrics can legitimately be zero for excluded models, so
    /// they are not sufficient to independently recompute event identity.
    pub digest_token_usage: RemoteTokenUsage,
    pub request_usage_exact: bool,
    pub exact_event_identity: bool,
    pub metrics: RemoteSessionUsageMetrics,
}

impl RemoteUsageEventFact {
    fn validate(
        &self,
        expected_thread: &ThreadId,
        revisions: &ProtocolRevisions,
        observed_at: DateTime<Utc>,
    ) -> Result<(), RemoteProtocolError> {
        if &self.emitting_thread_id != expected_thread {
            return Err(invalid_message(
                "remote usage fact belongs to a different emitting thread",
            ));
        }
        if self.occurred_at > observed_at {
            return Err(invalid_message(
                "remote usage fact occurs after the response observation",
            ));
        }
        if self.parent_thread_id.as_ref() == Some(&self.emitting_thread_id) {
            return Err(invalid_message(
                "remote usage fact cannot name itself as its parent",
            ));
        }
        if self
            .project_session_thread_id
            .as_ref()
            .is_some_and(|session| session != &self.root_session_thread_id)
            || (self.project_session_thread_id.is_none()
                && &self.root_session_thread_id != expected_thread)
        {
            return Err(invalid_message(
                "fact project session does not match its root-session fallback",
            ));
        }
        validate_optional_protocol_text(
            self.emitting_turn_id.as_deref(),
            MAX_TURN_ID_BYTES,
            "fact emitting turn ID",
        )?;
        validate_optional_protocol_text(
            self.root_session_turn_id.as_deref(),
            MAX_TURN_ID_BYTES,
            "fact root session turn ID",
        )?;
        validate_optional_protocol_text(self.model.as_deref(), MAX_MODEL_BYTES, "fact model")?;
        validate_optional_protocol_text(
            self.service_tier.as_deref(),
            MAX_SERVICE_TIER_BYTES,
            "fact service tier",
        )?;
        self.digest_token_usage.validate()?;
        if self.metrics.call_count != 1 {
            return Err(invalid_message(
                "remote usage fact must represent exactly one usage call",
            ));
        }
        self.metrics
            .validate_for_revisions(revisions, "usage event fact")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "changeKind",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteUsageEventFactMutation {
    Upsert(Box<RemoteUsageEventFact>),
    Tombstone,
}

/// Revisioned event record shared by snapshot and delta payloads. Snapshot
/// pages permit only upserts; delta pages may also carry tombstones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteUsageEventFactRecord {
    pub event_id: UsageEventId,
    pub occurred_at: DateTime<Utc>,
    pub revision: NonZeroU64,
    pub mutation: RemoteUsageEventFactMutation,
}

impl RemoteUsageEventFactRecord {
    fn validate(
        &self,
        expected_thread: &ThreadId,
        revisions: &ProtocolRevisions,
        observed_at: DateTime<Utc>,
    ) -> Result<(), RemoteProtocolError> {
        if self.occurred_at > observed_at {
            return Err(invalid_message(
                "remote usage fact record occurs after the response observation",
            ));
        }
        if let RemoteUsageEventFactMutation::Upsert(fact) = &self.mutation {
            fact.validate(expected_thread, revisions, observed_at)?;
            if fact.event_id != self.event_id || fact.occurred_at != self.occurred_at {
                return Err(invalid_message(
                    "remote usage fact record key does not match its payload",
                ));
            }
        }
        Ok(())
    }
}

/// One sequence-bearing transition in a per-thread fact journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteUsageEventFactDeltaChange {
    pub sequence: NonZeroU64,
    pub record: RemoteUsageEventFactRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteFactSnapshotPayload {
    pub fact_schema_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<RemoteUsageEventFactRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteFactDeltaPayload {
    pub fact_schema_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<RemoteUsageEventFactDeltaChange>,
}

/// Snapshot and delta fact payloads remain distinguishable after generic
/// response decoding; a payload kind cannot be substituted for the other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "factPayloadKind",
    content = "factPayload",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteSessionFactPayload {
    Snapshot(RemoteFactSnapshotPayload),
    Delta(RemoteFactDeltaPayload),
}

pub enum RemoteFactPageContext<'a> {
    Snapshot(&'a FactSnapshotPage),
    Delta(&'a FactDeltaPage),
}

pub struct RemoteFactPayloadContext<'a> {
    pub page: RemoteFactPageContext<'a>,
    pub request: Option<&'a SessionFactsRequest>,
    pub revisions: &'a ProtocolRevisions,
    pub observed_at: DateTime<Utc>,
}

impl RemotePagePayload for RemoteSessionFactPayload {
    fn validate_remote_payload(&self) -> Result<(), RemoteProtocolError> {
        // A fact payload is meaningful only with its page/envelope context.
        Ok(())
    }

    fn validate_remote_fact_payload(
        &self,
        context: &RemoteFactPayloadContext<'_>,
    ) -> Result<(), RemoteProtocolError> {
        self.validate(context)
    }
}

impl RemoteSessionFactPayload {
    fn validate(&self, context: &RemoteFactPayloadContext<'_>) -> Result<(), RemoteProtocolError> {
        match (self, &context.page) {
            (Self::Snapshot(payload), RemoteFactPageContext::Snapshot(page)) => {
                validate_fact_schema_version(payload.fact_schema_version)?;
                validate_count(
                    payload.records.len(),
                    MAX_SESSION_FACT_RECORDS_PER_PAGE,
                    "remote snapshot fact records",
                )?;
                for record in &payload.records {
                    if matches!(record.mutation, RemoteUsageEventFactMutation::Tombstone) {
                        return Err(invalid_message(
                            "remote fact snapshot cannot contain tombstones",
                        ));
                    }
                    if record.revision.get() > page.snapshot_watermark {
                        return Err(invalid_message(
                            "remote snapshot fact revision exceeds its watermark",
                        ));
                    }
                    record.validate(&page.thread_id, context.revisions, context.observed_at)?;
                }
                if payload
                    .records
                    .windows(2)
                    .any(|records| records[0].event_id.as_str() >= records[1].event_id.as_str())
                {
                    return Err(invalid_message(
                        "remote snapshot fact records must be sorted and unique",
                    ));
                }
                Ok(())
            }
            (Self::Delta(payload), RemoteFactPageContext::Delta(page)) => {
                validate_fact_schema_version(payload.fact_schema_version)?;
                validate_count(
                    payload.changes.len(),
                    MAX_SESSION_FACT_RECORDS_PER_PAGE,
                    "remote delta fact changes",
                )?;
                let requested_cursor = context.request.and_then(|request| match request.position {
                    SessionFactsPosition::DeltaStart { fact_cursor }
                    | SessionFactsPosition::DeltaContinue { fact_cursor, .. } => Some(fact_cursor),
                    SessionFactsPosition::SnapshotStart
                    | SessionFactsPosition::SnapshotContinue { .. } => None,
                });
                for change in &payload.changes {
                    if change.sequence.get() > page.delta_watermark
                        || change.record.revision != change.sequence
                        || requested_cursor
                            .is_some_and(|cursor| change.sequence.get() <= cursor.through_sequence)
                    {
                        return Err(invalid_message(
                            "remote delta fact sequence is outside its cursor range",
                        ));
                    }
                    change.record.validate(
                        &page.thread_id,
                        context.revisions,
                        context.observed_at,
                    )?;
                }
                if payload
                    .changes
                    .windows(2)
                    .any(|changes| changes[0].sequence >= changes[1].sequence)
                {
                    return Err(invalid_message(
                        "remote delta fact changes must be sorted by sequence",
                    ));
                }
                Ok(())
            }
            _ => Err(invalid_message(
                "remote fact payload kind does not match its response page",
            )),
        }
    }
}

fn validate_fact_schema_version(version: u32) -> Result<(), RemoteProtocolError> {
    if version != REMOTE_SESSION_FACT_SCHEMA_VERSION {
        return Err(invalid_message(
            "remote session fact schema version is unsupported",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteSessionDigest {
    pub thread_id: ThreadId,
    pub range_start: DateTime<Utc>,
    pub range_end: DateTime<Utc>,
    pub covered_through: DateTime<Utc>,
    pub fingerprint: RemoteSessionDigestFingerprint,
    pub project_breakdown_fingerprint: RemoteSessionDigestFingerprint,
    pub event_count: u64,
    pub exact_event_identity: bool,
    pub coverage_complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_project_keys: Vec<ObservedProjectKey>,
    pub metrics: RemoteSessionUsageMetrics,
}

impl RemoteSessionDigest {
    fn validate(&self, context: &RemoteDeltaPayloadContext<'_>) -> Result<(), RemoteProtocolError> {
        if self.range_end <= self.range_start
            || self.covered_through < self.range_start
            || self.covered_through > self.range_end
            || self.range_end.signed_duration_since(self.range_start)
                > Duration::days(MAX_EXPORT_RANGE_DAYS)
        {
            return Err(invalid_message("remote session digest range is invalid"));
        }
        if self.coverage_complete && self.covered_through != self.range_end {
            return Err(invalid_message(
                "complete remote session digest does not cover its range",
            ));
        }
        if self.event_count == 0 && !self.metrics.token_usage.is_zero() {
            return Err(invalid_message(
                "nonzero remote session digest has no events",
            ));
        }
        if self.metrics.call_count > self.event_count {
            return Err(invalid_message(
                "remote session digest call count exceeds its event count",
            ));
        }
        if self
            .observed_project_keys
            .windows(2)
            .any(|keys| keys[0].as_str() >= keys[1].as_str())
        {
            return Err(invalid_message(
                "remote session digest project keys must be sorted and unique",
            ));
        }
        self.metrics.validate(context)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "changeKind",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteSessionDigestMutation {
    Upsert(Box<RemoteSessionDigest>),
    Tombstone,
}

/// One journaled revision of `(source, profile, threadId, rangeStart)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteSessionDigestChange {
    pub sequence: NonZeroU64,
    pub thread_id: ThreadId,
    pub range_start: DateTime<Utc>,
    pub range_end: DateTime<Utc>,
    pub changed_at: DateTime<Utc>,
    pub retention_through: DateTime<Utc>,
    pub revision: NonZeroU64,
    pub mutation: RemoteSessionDigestMutation,
}

impl RemoteSessionDigestChange {
    fn validate(&self, context: &RemoteDeltaPayloadContext<'_>) -> Result<(), RemoteProtocolError> {
        if self.range_end <= self.range_start
            || self.changed_at < self.range_start
            || self.changed_at > context.observed_at
            || self.retention_through < self.range_end
            || self.retention_through < self.changed_at
        {
            return Err(invalid_message(
                "remote session digest change bounds are invalid",
            ));
        }
        if let RemoteSessionDigestMutation::Upsert(digest) = &self.mutation {
            digest.validate(context)?;
            if digest.thread_id != self.thread_id
                || digest.range_start != self.range_start
                || digest.range_end != self.range_end
                || digest.covered_through != self.changed_at
            {
                return Err(invalid_message(
                    "remote session digest change key does not match its payload",
                ));
            }
        }
        validate_change_sequence(self.sequence, context.page)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteLiveTask {
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_project_key: Option<ObservedProjectKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub status: TaskStatus,
    pub token_usage: RemoteTokenUsage,
    pub turn_count: u32,
}

impl RemoteLiveTask {
    fn validate(
        &self,
        captured_at: DateTime<Utc>,
        profile: RedactionProfile,
    ) -> Result<(), RemoteProtocolError> {
        if self.updated_at > captured_at
            || self
                .created_at
                .is_some_and(|created| created > self.updated_at)
        {
            return Err(invalid_message("remote live task timestamps are invalid"));
        }
        validate_preview(self.title_preview.as_deref(), profile, "live task title")?;
        self.token_usage.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteLiveTurn {
    pub thread_id: ThreadId,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    pub status: TurnStatus,
    pub token_usage: RemoteTokenUsage,
}

impl RemoteLiveTurn {
    fn validate(
        &self,
        captured_at: DateTime<Utc>,
        profile: RedactionProfile,
    ) -> Result<(), RemoteProtocolError> {
        validate_required_protocol_text(&self.turn_id, MAX_TURN_ID_BYTES, "live turn ID")?;
        validate_optional_protocol_text(self.model.as_deref(), MAX_MODEL_BYTES, "live model")?;
        validate_optional_protocol_text(
            self.reasoning_effort.as_deref(),
            MAX_REASONING_EFFORT_BYTES,
            "live reasoning effort",
        )?;
        validate_optional_protocol_text(
            self.service_tier.as_deref(),
            MAX_SERVICE_TIER_BYTES,
            "live service tier",
        )?;
        validate_preview(
            self.message_preview.as_deref(),
            profile,
            "live turn message",
        )?;
        if self.started_at.is_some_and(|started| started > captured_at)
            || self
                .completed_at
                .is_some_and(|completed| completed > captured_at)
            || matches!((self.started_at, self.completed_at), (Some(started), Some(completed)) if completed < started)
        {
            return Err(invalid_message("remote live turn timestamps are invalid"));
        }
        self.token_usage.validate()
    }

    fn sort_key(&self) -> (&str, &str) {
        (self.thread_id.as_str(), self.turn_id.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteLiveSnapshot {
    pub captured_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<RemoteLiveTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<RemoteLiveTurn>,
}

impl RemoteLiveSnapshot {
    fn validate(&self, context: &RemoteDeltaPayloadContext<'_>) -> Result<(), RemoteProtocolError> {
        if self.captured_at > context.observed_at {
            return Err(invalid_message(
                "remote live capture time follows response observation",
            ));
        }
        validate_count(self.tasks.len(), MAX_LIVE_TASKS, "remote live tasks")?;
        validate_count(self.turns.len(), MAX_LIVE_TURNS, "remote live turns")?;
        for task in &self.tasks {
            task.validate(self.captured_at, context.redaction_profile)?;
        }
        if self
            .tasks
            .windows(2)
            .any(|tasks| tasks[0].thread_id.as_str() >= tasks[1].thread_id.as_str())
        {
            return Err(invalid_message(
                "remote live tasks must be sorted and unique by thread ID",
            ));
        }
        for turn in &self.turns {
            turn.validate(self.captured_at, context.redaction_profile)?;
        }
        if self
            .turns
            .windows(2)
            .any(|turns| turns[0].sort_key() >= turns[1].sort_key())
        {
            return Err(invalid_message(
                "remote live turns must be sorted and unique",
            ));
        }
        let task_threads = self
            .tasks
            .iter()
            .map(|task| task.thread_id.as_str())
            .collect::<BTreeSet<_>>();
        if self
            .turns
            .iter()
            .any(|turn| !task_threads.contains(turn.thread_id.as_str()))
        {
            return Err(invalid_message(
                "remote live turn references a missing task",
            ));
        }
        Ok(())
    }

    /// Revalidates a snapshot loaded from a local durable cache without
    /// manufacturing a transport page. The capture instant is the strongest
    /// observation bound available in that cache; envelope validation already
    /// checked it against the remote response before publication.
    pub(crate) fn validate_for_storage(
        &self,
        profile: RedactionProfile,
    ) -> Result<(), RemoteProtocolError> {
        validate_count(self.tasks.len(), MAX_LIVE_TASKS, "remote live tasks")?;
        validate_count(self.turns.len(), MAX_LIVE_TURNS, "remote live turns")?;
        for task in &self.tasks {
            task.validate(self.captured_at, profile)?;
        }
        if self
            .tasks
            .windows(2)
            .any(|tasks| tasks[0].thread_id.as_str() >= tasks[1].thread_id.as_str())
        {
            return Err(invalid_message(
                "remote live tasks must be sorted and unique by thread ID",
            ));
        }
        for turn in &self.turns {
            turn.validate(self.captured_at, profile)?;
        }
        if self
            .turns
            .windows(2)
            .any(|turns| turns[0].sort_key() >= turns[1].sort_key())
        {
            return Err(invalid_message(
                "remote live turns must be sorted and unique",
            ));
        }
        let task_threads = self
            .tasks
            .iter()
            .map(|task| task.thread_id.as_str())
            .collect::<BTreeSet<_>>();
        if self
            .turns
            .iter()
            .any(|turn| !task_threads.contains(turn.thread_id.as_str()))
        {
            return Err(invalid_message(
                "remote live turn references a missing task",
            ));
        }
        Ok(())
    }
}

/// Revision-only means unchanged since the center's cached copy; `snapshot:
/// Some` atomically replaces that copy, including when both row lists are empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteLiveState {
    pub live_revision: NonZeroU64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<RemoteLiveSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteDeltaWarning {
    pub code: String,
    pub occurrences: NonZeroU64,
}

impl RemoteDeltaWarning {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        validate_machine_code(&self.code, MAX_WARNING_CODE_BYTES, "remote warning code")
    }

    pub(crate) fn validate_for_storage(&self) -> Result<(), RemoteProtocolError> {
        self.validate()
    }
}

pub(crate) fn validate_remote_partial_reasons_for_storage(
    reasons: &[String],
) -> Result<(), RemoteProtocolError> {
    validate_partial_reasons(reasons, "live state")
}

/// Exact page counts plus bounded collection diagnostics. `journalRecordsScanned`
/// counts the contiguous cursor interval, including records filtered by range.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteDeltaStats {
    pub journal_records_scanned: u64,
    pub project_descriptors_emitted: u64,
    pub bucket_changes_emitted: u64,
    pub session_digest_changes_emitted: u64,
    pub live_tasks_emitted: u64,
    pub live_turns_emitted: u64,
    pub discovered_files: u64,
    pub parsed_files: u64,
    pub reused_files: u64,
    pub unreadable_files: u64,
    pub truncated_files: u64,
    pub skipped_lines: u64,
    pub ambiguous_token_resets: u64,
    pub warnings_suppressed: u64,
    pub git_commands_spawned: u64,
    pub git_workspaces_probed: u64,
    pub git_evidence_cache_hits: u64,
    pub git_budget_exhausted: u64,
    pub git_elapsed_millis: u64,
}

impl RemoteDeltaStats {
    fn validate(&self, payload: &DeltaPayload) -> Result<(), RemoteProtocolError> {
        let (live_tasks, live_turns) = payload
            .live
            .as_ref()
            .and_then(|live| live.snapshot.as_ref())
            .map(|snapshot| (snapshot.tasks.len(), snapshot.turns.len()))
            .unwrap_or_default();
        if self.project_descriptors_emitted != payload.project_descriptors.len() as u64
            || self.bucket_changes_emitted != payload.bucket_changes.len() as u64
            || self.session_digest_changes_emitted != payload.session_digest_changes.len() as u64
            || self.live_tasks_emitted != live_tasks as u64
            || self.live_turns_emitted != live_turns as u64
        {
            return Err(invalid_message(
                "remote delta stats do not match emitted payload counts",
            ));
        }
        let processed_files = self
            .parsed_files
            .checked_add(self.reused_files)
            .ok_or_else(|| invalid_message("remote processed file count overflows"))?;
        if self.parsed_files > self.discovered_files
            || self.reused_files > self.discovered_files
            || processed_files > self.discovered_files
            || self.unreadable_files > self.discovered_files
            || self.truncated_files > self.discovered_files
        {
            return Err(invalid_message(
                "remote delta collection file counts are inconsistent",
            ));
        }
        let emitted_changes = (payload.bucket_changes.len() as u64)
            .checked_add(payload.session_digest_changes.len() as u64)
            .ok_or_else(|| invalid_message("remote emitted change count overflows"))?;
        // The source/profile cursor is global and range-independent. Every
        // scanned journal transition must therefore be present in exactly one
        // typed change vector; allowing scanned-but-hidden records would make
        // a later wider query permanently incomplete.
        if emitted_changes != self.journal_records_scanned {
            return Err(invalid_message(
                "remote delta emitted changes do not match journal records scanned",
            ));
        }
        Ok(())
    }
}

/// Strict aggregate delta payload. It is self-contained for every referenced
/// observed project key and contains no center-owned project IDs or raw paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeltaPayload {
    pub coverage: RemoteDeltaCoverage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_descriptors: Vec<RemoteProjectDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bucket_changes: Vec<RemoteUsageBucketChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_digest_changes: Vec<RemoteSessionDigestChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<RemoteLiveState>,
    pub stats: RemoteDeltaStats,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<RemoteDeltaWarning>,
}

/// Fully typed aggregate response; fact pages deliberately remain a separate
/// payload domain and cannot be substituted for this alias.
pub type RemoteDeltaResponse = RemoteExportResponse<DeltaPayload, EmptyRemotePayload>;
/// Fully typed per-thread fact response. Aggregate payloads remain available
/// only for the other response variant.
pub type RemoteSessionFactResponse = RemoteExportResponse<DeltaPayload, RemoteSessionFactPayload>;

/// Context supplied by the response envelope and, during negotiated validation,
/// the exact request. Payload types must not carry duplicate source/profile keys.
pub struct RemoteDeltaPayloadContext<'a> {
    pub page: &'a DeltaPage,
    pub request: Option<&'a DeltaRequest>,
    pub source: &'a SourceGeneration,
    pub redaction_profile: RedactionProfile,
    pub revisions: &'a ProtocolRevisions,
    pub observed_at: DateTime<Utc>,
}

impl RemotePagePayload for DeltaPayload {
    fn validate_remote_payload(&self) -> Result<(), RemoteProtocolError> {
        // A delta payload is only meaningful with its page/envelope context.
        // RemoteExportResponse always invokes the contextual hook.
        Ok(())
    }

    fn validate_remote_delta_payload(
        &self,
        context: &RemoteDeltaPayloadContext<'_>,
    ) -> Result<(), RemoteProtocolError> {
        self.validate(context)
    }
}

impl DeltaPayload {
    fn validate(&self, context: &RemoteDeltaPayloadContext<'_>) -> Result<(), RemoteProtocolError> {
        let _source_namespace = context.source;
        self.coverage.validate()?;
        validate_count(
            self.project_descriptors.len(),
            MAX_PROJECT_DESCRIPTORS_PER_PAGE,
            "remote project descriptors",
        )?;
        validate_count(
            self.bucket_changes.len(),
            MAX_BUCKET_CHANGES_PER_PAGE,
            "remote bucket changes",
        )?;
        validate_count(
            self.session_digest_changes.len(),
            MAX_SESSION_DIGEST_CHANGES_PER_PAGE,
            "remote session digest changes",
        )?;
        validate_count(
            self.warnings.len(),
            MAX_WARNINGS_PER_PAGE,
            "remote warnings",
        )?;

        for descriptor in &self.project_descriptors {
            descriptor.validate()?;
        }
        if self.project_descriptors.windows(2).any(|descriptors| {
            descriptors[0].observed_project_key.as_str()
                >= descriptors[1].observed_project_key.as_str()
        }) {
            return Err(invalid_message(
                "remote project descriptors must be sorted and unique",
            ));
        }

        for change in &self.bucket_changes {
            change.validate(context)?;
        }
        validate_change_order(
            self.bucket_changes
                .iter()
                .map(|change| change.sequence.get()),
            "remote bucket changes",
        )?;
        for change in &self.session_digest_changes {
            change.validate(context)?;
        }
        validate_change_order(
            self.session_digest_changes
                .iter()
                .map(|change| change.sequence.get()),
            "remote session digest changes",
        )?;
        let mut all_sequences = BTreeSet::new();
        if self
            .bucket_changes
            .iter()
            .map(|change| change.sequence.get())
            .chain(
                self.session_digest_changes
                    .iter()
                    .map(|change| change.sequence.get()),
            )
            .any(|sequence| !all_sequences.insert(sequence))
        {
            return Err(invalid_message(
                "remote delta journal sequences must be globally unique",
            ));
        }

        if let Some(live) = &self.live
            && let Some(snapshot) = &live.snapshot
        {
            snapshot.validate(context)?;
        }
        for warning in &self.warnings {
            warning.validate()?;
        }
        if self
            .warnings
            .windows(2)
            .any(|warnings| warnings[0].code >= warnings[1].code)
        {
            return Err(invalid_message(
                "remote warnings must be sorted and unique by code",
            ));
        }
        self.stats.validate(self)?;
        self.validate_descriptor_references()?;
        self.validate_live_wire_budget()?;

        if let Some(request) = context.request {
            self.validate_for_request(request, context.page)?;
        }
        Ok(())
    }

    fn validate_descriptor_references(&self) -> Result<(), RemoteProtocolError> {
        let descriptors = self
            .project_descriptors
            .iter()
            .map(|descriptor| descriptor.observed_project_key.as_str())
            .collect::<BTreeSet<_>>();
        let referenced = self
            .bucket_changes
            .iter()
            .filter_map(|change| match &change.mutation {
                RemoteUsageBucketMutation::Upsert(bucket) => Some(bucket.as_ref()),
                RemoteUsageBucketMutation::Tombstone => None,
            })
            .flat_map(|bucket| {
                bucket
                    .project_groups
                    .iter()
                    .filter_map(|group| group.observed_project_key.as_ref())
            })
            .chain(
                self.session_digest_changes
                    .iter()
                    .filter_map(|change| match &change.mutation {
                        RemoteSessionDigestMutation::Upsert(digest) => Some(digest.as_ref()),
                        RemoteSessionDigestMutation::Tombstone => None,
                    })
                    .flat_map(|digest| digest.observed_project_keys.iter()),
            )
            .chain(
                self.live
                    .iter()
                    .filter_map(|live| live.snapshot.as_ref())
                    .flat_map(|snapshot| snapshot.tasks.iter())
                    .filter_map(|task| task.observed_project_key.as_ref()),
            )
            .map(ObservedProjectKey::as_str)
            .collect::<BTreeSet<_>>();
        if referenced != descriptors {
            return Err(invalid_message(
                "remote delta project descriptors must exactly match page-local project references",
            ));
        }
        Ok(())
    }

    fn validate_live_wire_budget(&self) -> Result<(), RemoteProtocolError> {
        let Some(snapshot) = self.live.as_ref().and_then(|live| live.snapshot.as_ref()) else {
            return Ok(());
        };
        let referenced = snapshot
            .tasks
            .iter()
            .filter_map(|task| task.observed_project_key.as_ref())
            .map(ObservedProjectKey::as_str)
            .collect::<BTreeSet<_>>();
        let descriptors = self
            .project_descriptors
            .iter()
            .filter(|descriptor| referenced.contains(descriptor.observed_project_key.as_str()))
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec(&(&snapshot.tasks, &snapshot.turns, &descriptors))
            .map_err(|_| invalid_message("remote live state cannot be serialized"))?;
        if encoded.len() > MAX_LIVE_SERIALIZED_BYTES {
            return Err(invalid_message(
                "remote live state exceeds its serialized byte bound",
            ));
        }
        Ok(())
    }

    fn validate_for_request(
        &self,
        request: &DeltaRequest,
        page: &DeltaPage,
    ) -> Result<(), RemoteProtocolError> {
        if self.coverage.requested_range != request.range {
            return Err(invalid_message(
                "remote delta coverage does not match its request range",
            ));
        }
        if request.include_live != self.live.is_some() {
            return Err(invalid_message(
                "remote delta live payload does not match includeLive",
            ));
        }
        if let Some(live) = self.live.as_ref() {
            match (
                request.known_live_revision == Some(live.live_revision),
                live.snapshot.is_some(),
            ) {
                (true, false) | (false, true) => {}
                (true, true) => {
                    return Err(invalid_message(
                        "remote delta resent an already-known live revision",
                    ));
                }
                (false, false) => {
                    return Err(invalid_message(
                        "remote delta omitted an unknown live replacement",
                    ));
                }
            }
        }
        let scanned = if let Some(cursor) = request.delta_cursor {
            page.through_sequence
                .checked_sub(cursor.sequence)
                .ok_or_else(|| invalid_message("remote delta cursor regressed"))?
        } else if self.stats.journal_records_scanned == 0 {
            if page.from_sequence != page.through_sequence {
                return Err(invalid_message(
                    "empty remote bootstrap page advances its sequence range",
                ));
            }
            0
        } else {
            page.through_sequence
                .checked_sub(page.from_sequence)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| invalid_message("remote bootstrap sequence range is invalid"))?
        };
        if self.stats.journal_records_scanned != scanned {
            return Err(invalid_message(
                "remote scanned journal count does not match cursor progress",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FactSnapshotPage {
    pub thread_id: ThreadId,
    pub snapshot_id: FactSnapshotId,
    pub fact_generation: NonZeroU64,
    pub snapshot_watermark: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<FactSnapshotPageToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activate_fact_cursor: Option<FactCursor>,
    pub has_more: bool,
}

impl FactSnapshotPage {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        validate_fact_page_completion(
            self.fact_generation,
            self.snapshot_watermark,
            self.has_more,
            self.next_page_token.is_some(),
            self.activate_fact_cursor,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FactDeltaPage {
    pub thread_id: ThreadId,
    pub batch_id: FactBatchId,
    pub fact_generation: NonZeroU64,
    pub delta_watermark: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<FactDeltaPageToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activate_fact_cursor: Option<FactCursor>,
    pub has_more: bool,
}

impl FactDeltaPage {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        validate_fact_page_completion(
            self.fact_generation,
            self.delta_watermark,
            self.has_more,
            self.next_page_token.is_some(),
            self.activate_fact_cursor,
        )
    }
}

fn validate_fact_page_completion(
    generation: NonZeroU64,
    watermark: u64,
    has_more: bool,
    has_next_token: bool,
    activate_cursor: Option<FactCursor>,
) -> Result<(), RemoteProtocolError> {
    if has_more {
        if !has_next_token || activate_cursor.is_some() {
            return Err(invalid_message(
                "intermediate fact page must have only a continuation token",
            ));
        }
    } else if has_next_token || activate_cursor.is_none() {
        return Err(invalid_message(
            "final fact page must have only an activation cursor",
        ));
    }
    if let Some(cursor) = activate_cursor
        && (cursor.fact_generation != generation || cursor.through_sequence != watermark)
    {
        return Err(invalid_message(
            "fact activation cursor does not match its page watermark",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFailureKind {
    VersionMismatch,
    IdentityMismatch,
    RedactionMismatch,
    CursorExpired,
    FactCursorExpired,
    FactEvidenceUnavailable,
    FactDigestChanged,
    FactInventoryTooLarge,
    Busy,
    InvalidRequest,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteFailure {
    pub kind: RemoteFailureKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
}

impl RemoteFailure {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        if self.message.is_empty()
            || self.message.chars().count() > MAX_ERROR_MESSAGE_CHARS
            || self.message.trim() != self.message
            || self.message.chars().any(|character| {
                character.is_control() || matches!(character, '\u{2028}' | '\u{2029}')
            })
        {
            return Err(invalid_message("remote error message is invalid"));
        }
        if self.retry_after_seconds.is_some() && self.kind != RemoteFailureKind::Busy {
            return Err(invalid_message(
                "only busy failures may carry retryAfterSeconds",
            ));
        }
        Ok(())
    }
}

/// Response result. Aggregate and fact payloads stay strongly separated even
/// before their storage schemas are connected to the exporter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "responseKind",
    content = "response",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteExportResponseBody<D = EmptyRemotePayload, F = EmptyRemotePayload> {
    Probe(ProbeResult),
    Delta { page: DeltaPage, payload: D },
    FactSnapshot { page: FactSnapshotPage, payload: F },
    FactDelta { page: FactDeltaPage, payload: F },
    Failure(RemoteFailure),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteTiming {
    pub remote_received_at: DateTime<Utc>,
    pub remote_sent_at: DateTime<Utc>,
}

impl RemoteTiming {
    fn validate(&self) -> Result<(), RemoteProtocolError> {
        let duration = self
            .remote_sent_at
            .signed_duration_since(self.remote_received_at);
        if duration < Duration::zero() || duration > MAX_REMOTE_OPERATION_DURATION {
            return Err(invalid_message("remote response timing is invalid"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExportResponse<D = EmptyRemotePayload, F = EmptyRemotePayload> {
    pub protocol_version: u32,
    pub server_version: BinaryVersion,
    pub source: SourceGeneration,
    pub redaction_profile: RedactionProfile,
    pub revisions: ProtocolRevisions,
    pub observed_at: DateTime<Utc>,
    pub timing: RemoteTiming,
    pub result: RemoteExportResponseBody<D, F>,
}

impl<D, F> RemoteExportResponse<D, F>
where
    D: RemotePagePayload,
    F: RemotePagePayload,
{
    /// Validates both messages and the negotiated relationship between them.
    ///
    /// A failure result is allowed to report a mismatched observed identity or
    /// revision set so the center can present the reason. Successful results
    /// must match the pinned identity, requested redaction namespace, accepted
    /// revisions, and request kind exactly.
    pub fn validate_for_request(
        &self,
        request: &RemoteExportRequest,
    ) -> Result<(), RemoteProtocolError> {
        request.validate_remote_protocol()?;
        self.validate_remote_protocol()?;
        if matches!(&self.result, RemoteExportResponseBody::Failure(_)) {
            return Ok(());
        }
        if self.redaction_profile != request.redaction_profile {
            return Err(invalid_message(
                "remote response redaction profile does not match its request",
            ));
        }
        if let Some(expected_source) = &request.expected_source
            && expected_source != &self.source
        {
            return Err(invalid_message(
                "remote response source identity does not match its pinned request",
            ));
        }
        if !request.accepted_revisions.accepts(&self.revisions) {
            return Err(invalid_message(
                "remote response revisions are outside the accepted ranges",
            ));
        }

        match (&request.request, &self.result) {
            (RemoteExportRequestBody::Probe(_), RemoteExportResponseBody::Probe(_)) => Ok(()),
            (
                RemoteExportRequestBody::Delta(request),
                RemoteExportResponseBody::Delta { page, payload },
            ) => {
                validate_delta_page_for_request(request, page)?;
                payload.validate_remote_delta_payload(&RemoteDeltaPayloadContext {
                    page,
                    request: Some(request),
                    source: &self.source,
                    redaction_profile: self.redaction_profile,
                    revisions: &self.revisions,
                    observed_at: self.observed_at,
                })
            }
            (
                RemoteExportRequestBody::SessionFacts(request),
                RemoteExportResponseBody::FactSnapshot { page, payload },
            ) => {
                validate_fact_snapshot_page_for_request(request, page)?;
                payload.validate_remote_fact_payload(&RemoteFactPayloadContext {
                    page: RemoteFactPageContext::Snapshot(page),
                    request: Some(request),
                    revisions: &self.revisions,
                    observed_at: self.observed_at,
                })
            }
            (
                RemoteExportRequestBody::SessionFacts(request),
                RemoteExportResponseBody::FactDelta { page, payload },
            ) => {
                validate_fact_delta_page_for_request(request, page)?;
                payload.validate_remote_fact_payload(&RemoteFactPayloadContext {
                    page: RemoteFactPageContext::Delta(page),
                    request: Some(request),
                    revisions: &self.revisions,
                    observed_at: self.observed_at,
                })
            }
            _ => Err(invalid_message(
                "remote response kind does not match its request",
            )),
        }
    }
}

fn validate_delta_page_for_request(
    request: &DeltaRequest,
    page: &DeltaPage,
) -> Result<(), RemoteProtocolError> {
    let Some(cursor) = request.delta_cursor else {
        return Ok(());
    };
    if page.generation != cursor.generation || page.through_sequence < cursor.sequence {
        return Err(invalid_message(
            "delta response generation or watermark does not continue its request cursor",
        ));
    }
    if page.through_sequence == cursor.sequence {
        if page.from_sequence != cursor.sequence || page.has_more {
            return Err(invalid_message(
                "an unchanged delta response must be a final zero-progress page",
            ));
        }
        return Ok(());
    }
    let expected_from = cursor
        .sequence
        .checked_add(1)
        .ok_or_else(|| invalid_message("delta request cursor cannot advance"))?;
    if page.from_sequence != expected_from {
        return Err(invalid_message(
            "delta response does not begin after its request cursor",
        ));
    }
    Ok(())
}

fn validate_fact_snapshot_page_for_request(
    request: &SessionFactsRequest,
    page: &FactSnapshotPage,
) -> Result<(), RemoteProtocolError> {
    if page.thread_id != request.thread_id {
        return Err(invalid_message(
            "fact snapshot response thread does not match its request",
        ));
    }
    match &request.position {
        SessionFactsPosition::SnapshotStart => Ok(()),
        SessionFactsPosition::SnapshotContinue {
            snapshot_id,
            fact_generation,
            snapshot_watermark,
            ..
        } if snapshot_id == &page.snapshot_id
            && fact_generation == &page.fact_generation
            && snapshot_watermark == &page.snapshot_watermark =>
        {
            Ok(())
        }
        SessionFactsPosition::SnapshotContinue { .. } => Err(invalid_message(
            "fact snapshot continuation changed snapshot context",
        )),
        SessionFactsPosition::DeltaStart { .. } | SessionFactsPosition::DeltaContinue { .. } => {
            Err(invalid_message(
                "fact snapshot response does not match a delta request",
            ))
        }
    }
}

fn validate_fact_delta_page_for_request(
    request: &SessionFactsRequest,
    page: &FactDeltaPage,
) -> Result<(), RemoteProtocolError> {
    if page.thread_id != request.thread_id {
        return Err(invalid_message(
            "fact delta response thread does not match its request",
        ));
    }
    match &request.position {
        SessionFactsPosition::DeltaStart { fact_cursor } => {
            validate_fact_delta_cursor(*fact_cursor, page)
        }
        SessionFactsPosition::DeltaContinue {
            fact_cursor,
            batch_id,
            delta_watermark,
            ..
        } => {
            validate_fact_delta_cursor(*fact_cursor, page)?;
            if batch_id != &page.batch_id || delta_watermark != &page.delta_watermark {
                return Err(invalid_message(
                    "fact delta continuation changed batch context",
                ));
            }
            Ok(())
        }
        SessionFactsPosition::SnapshotStart | SessionFactsPosition::SnapshotContinue { .. } => Err(
            invalid_message("fact delta response does not match a snapshot request"),
        ),
    }
}

fn validate_fact_delta_cursor(
    cursor: FactCursor,
    page: &FactDeltaPage,
) -> Result<(), RemoteProtocolError> {
    if page.fact_generation != cursor.fact_generation
        || page.delta_watermark < cursor.through_sequence
        || (page.has_more && page.delta_watermark == cursor.through_sequence)
    {
        return Err(invalid_message(
            "fact delta response does not advance its request cursor",
        ));
    }
    Ok(())
}

impl<D, F> RemoteProtocolMessage for RemoteExportResponse<D, F>
where
    D: RemotePagePayload,
    F: RemotePagePayload,
{
    fn validate_remote_protocol(&self) -> Result<(), RemoteProtocolError> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(invalid_message(
                "remote response protocol version is unsupported",
            ));
        }
        self.timing.validate()?;
        if self.observed_at < self.timing.remote_received_at
            || self.observed_at > self.timing.remote_sent_at
        {
            return Err(invalid_message(
                "remote observedAt lies outside response timing",
            ));
        }
        match &self.result {
            RemoteExportResponseBody::Probe(result) => result.validate(),
            RemoteExportResponseBody::Delta { page, payload } => {
                page.validate()?;
                payload.validate_remote_payload()?;
                payload.validate_remote_delta_payload(&RemoteDeltaPayloadContext {
                    page,
                    request: None,
                    source: &self.source,
                    redaction_profile: self.redaction_profile,
                    revisions: &self.revisions,
                    observed_at: self.observed_at,
                })
            }
            RemoteExportResponseBody::FactSnapshot { page, payload } => {
                page.validate()?;
                payload.validate_remote_payload()?;
                payload.validate_remote_fact_payload(&RemoteFactPayloadContext {
                    page: RemoteFactPageContext::Snapshot(page),
                    request: None,
                    revisions: &self.revisions,
                    observed_at: self.observed_at,
                })
            }
            RemoteExportResponseBody::FactDelta { page, payload } => {
                page.validate()?;
                payload.validate_remote_payload()?;
                payload.validate_remote_fact_payload(&RemoteFactPayloadContext {
                    page: RemoteFactPageContext::Delta(page),
                    request: None,
                    revisions: &self.revisions,
                    observed_at: self.observed_at,
                })
            }
            RemoteExportResponseBody::Failure(failure) => failure.validate(),
        }
    }
}

fn validate_prefixed_lower_hex(
    value: &str,
    prefix: &str,
    hex_bytes: usize,
    subject: &str,
) -> Result<(), RemoteProtocolError> {
    let Some(hex) = value.strip_prefix(prefix) else {
        return Err(invalid_message(format!("{subject} has the wrong prefix")));
    };
    if hex.len() != hex_bytes
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_message(format!(
            "{subject} must contain exactly {hex_bytes} lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn validate_repository_relative_root(root: &str) -> Result<(), RemoteProtocolError> {
    if root == "." {
        return Ok(());
    }
    if root.is_empty()
        || root.len() > MAX_REPOSITORY_RELATIVE_ROOT_BYTES
        || root.starts_with('/')
        || root.contains('\\')
        || root.chars().any(|character| {
            character.is_control()
                || is_bidi_control(character)
                || matches!(character, '\u{2028}' | '\u{2029}')
        })
        || root
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid_message(
            "repository-relative workspace root is invalid",
        ));
    }
    Ok(())
}

fn validate_aligned_bucket_start(starts_at: DateTime<Utc>) -> Result<(), RemoteProtocolError> {
    if starts_at.timestamp_subsec_nanos() != 0
        || starts_at.timestamp().rem_euclid(SOURCE_BUCKET_MINUTES * 60) != 0
    {
        return Err(invalid_message(
            "remote bucket start must be UTC-aligned to 15 minutes",
        ));
    }
    Ok(())
}

fn validate_bucket_bounds(
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
) -> Result<(), RemoteProtocolError> {
    validate_aligned_bucket_start(starts_at)?;
    if starts_at.checked_add_signed(Duration::minutes(SOURCE_BUCKET_MINUTES)) != Some(ends_at) {
        return Err(invalid_message(
            "remote bucket must span exactly 15 minutes",
        ));
    }
    Ok(())
}

fn validate_required_protocol_text(
    value: &str,
    maximum_bytes: usize,
    subject: &str,
) -> Result<(), RemoteProtocolError> {
    validate_optional_protocol_text(Some(value), maximum_bytes, subject)
}

fn validate_optional_protocol_text(
    value: Option<&str>,
    maximum_bytes: usize,
    subject: &str,
) -> Result<(), RemoteProtocolError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty()
        || value.trim() != value
        || value.len() > maximum_bytes
        || value.chars().any(|character| {
            character.is_control()
                || is_bidi_control(character)
                || matches!(character, '\u{2028}' | '\u{2029}')
        })
    {
        return Err(invalid_message(format!("remote {subject} is invalid")));
    }
    Ok(())
}

fn validate_preview(
    value: Option<&str>,
    profile: RedactionProfile,
    subject: &str,
) -> Result<(), RemoteProtocolError> {
    if profile == RedactionProfile::Redacted && value.is_some() {
        return Err(invalid_message(format!(
            "redacted remote payload contains {subject}"
        )));
    }
    let Some(value) = value else {
        return Ok(());
    };
    if value.chars().count() > MAX_PREVIEW_CHARS {
        return Err(invalid_message(format!("remote {subject} is too long")));
    }
    validate_optional_protocol_text(Some(value), MAX_PREVIEW_BYTES, subject)
}

fn validate_machine_code(
    value: &str,
    maximum_bytes: usize,
    subject: &str,
) -> Result<(), RemoteProtocolError> {
    if value.is_empty()
        || value.len() > maximum_bytes
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.' | b':')
        })
    {
        return Err(invalid_message(format!("{subject} is invalid")));
    }
    Ok(())
}

fn validate_partial_reasons(reasons: &[String], subject: &str) -> Result<(), RemoteProtocolError> {
    validate_count(reasons.len(), MAX_PARTIAL_REASONS, "remote partial reasons")?;
    for reason in reasons {
        validate_machine_code(reason, MAX_PARTIAL_REASON_BYTES, "remote partial reason")?;
    }
    if reasons.windows(2).any(|reasons| reasons[0] >= reasons[1]) {
        return Err(invalid_message(format!(
            "remote {subject} partial reasons must be sorted and unique"
        )));
    }
    Ok(())
}

fn validate_count(count: usize, maximum: usize, subject: &str) -> Result<(), RemoteProtocolError> {
    if count > maximum {
        return Err(invalid_message(format!(
            "{subject} exceed the maximum count of {maximum}"
        )));
    }
    Ok(())
}

fn validate_usage_count(
    call_count: u64,
    token_usage: RemoteTokenUsage,
    subject: &str,
) -> Result<(), RemoteProtocolError> {
    if call_count == 0 && !token_usage.is_zero() {
        return Err(invalid_message(format!(
            "nonzero remote {subject} usage requires at least one call"
        )));
    }
    Ok(())
}

fn validate_revisions(
    metric: NonZeroU32,
    estimator: NonZeroU32,
    project_breakdown: NonZeroU32,
    api_pricing_catalog: NonZeroU32,
    revisions: &ProtocolRevisions,
    subject: &str,
) -> Result<(), RemoteProtocolError> {
    if metric != revisions.metric
        || estimator != revisions.estimator
        || project_breakdown != revisions.project_breakdown
        || api_pricing_catalog != revisions.api_pricing_catalog
    {
        return Err(invalid_message(format!(
            "remote {subject} revisions do not match the response envelope"
        )));
    }
    Ok(())
}

fn validate_change_sequence(
    sequence: NonZeroU64,
    page: &DeltaPage,
) -> Result<(), RemoteProtocolError> {
    if sequence.get() < page.from_sequence || sequence.get() > page.through_sequence {
        return Err(invalid_message(
            "remote delta change sequence lies outside its page watermark",
        ));
    }
    Ok(())
}

fn validate_change_order(
    sequences: impl IntoIterator<Item = u64>,
    subject: &str,
) -> Result<(), RemoteProtocolError> {
    let mut previous = None;
    for sequence in sequences {
        if previous.is_some_and(|previous| sequence <= previous) {
            return Err(invalid_message(format!(
                "{subject} must be strictly ordered by sequence"
            )));
        }
        previous = Some(sequence);
    }
    Ok(())
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn invalid_message(message: impl Into<String>) -> RemoteProtocolError {
    RemoteProtocolError::new(RemoteProtocolErrorKind::InvalidMessage, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn nonzero32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn nonzero64(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).unwrap()
    }

    fn revisions() -> ProtocolRevisions {
        ProtocolRevisions {
            history_format: nonzero32(2),
            metric: nonzero32(3),
            estimator: nonzero32(4),
            project_breakdown: nonzero32(5),
            api_pricing_catalog: nonzero32(6),
        }
    }

    fn accepted_revisions() -> AcceptedRevisions {
        let exact = |value| AcceptedRevisionRange {
            min: nonzero32(value),
            max: nonzero32(value),
        };
        AcceptedRevisions {
            history_format: exact(2),
            metric: exact(3),
            estimator: exact(4),
            project_breakdown: exact(5),
            api_pricing_catalog: exact(6),
        }
    }

    fn source() -> SourceGeneration {
        SourceGeneration {
            node_id: "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
            generation: nonzero64(7),
        }
    }

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn observed_project_key(hex: u64) -> ObservedProjectKey {
        format!("opk-hmac-sha256-v1-{hex:064x}").parse().unwrap()
    }

    fn git_fingerprint(hex: u64) -> GitRepositoryFingerprint {
        format!("git-sha256-v1-{hex:064x}").parse().unwrap()
    }

    #[test]
    fn repository_root_descriptor_uses_explicit_dot_representation() {
        let mut descriptor = RemoteProjectDescriptor {
            observed_project_key: observed_project_key(1),
            display_label: "project".parse().unwrap(),
            git_evidence: RemoteGitRepositoryEvidence::Repository {
                fingerprint: Some(git_fingerprint(2)),
                repository_relative_workspace_root: ".".to_owned(),
            },
        };
        descriptor.validate().unwrap();
        descriptor.git_evidence = RemoteGitRepositoryEvidence::Repository {
            fingerprint: None,
            repository_relative_workspace_root: ".".to_owned(),
        };
        descriptor.validate().unwrap();
        let serialized = serde_json::to_value(&descriptor).unwrap();
        assert_eq!(serialized["gitEvidence"]["status"], "repository");
        assert_eq!(
            serialized["gitEvidence"]["repositoryRelativeWorkspaceRoot"],
            "."
        );

        descriptor.git_evidence = RemoteGitRepositoryEvidence::ConfirmedNonRepository;
        assert_eq!(
            serde_json::to_value(&descriptor).unwrap()["gitEvidence"]["status"],
            "confirmed_non_repository"
        );
        descriptor.git_evidence = RemoteGitRepositoryEvidence::Unavailable;
        assert_eq!(
            serde_json::to_value(&descriptor).unwrap()["gitEvidence"]["status"],
            "unavailable"
        );
    }

    fn digest_fingerprint(hex: u64) -> RemoteSessionDigestFingerprint {
        format!("session-digest-sha256-v1-{hex:064x}")
            .parse()
            .unwrap()
    }

    fn fact_digest_binding() -> SessionFactsDigestBinding {
        SessionFactsDigestBinding {
            range_start: at("2026-08-30T00:00:00Z"),
            range_end: at("2026-08-31T00:00:00Z"),
            covered_through: at("2026-08-30T02:00:00Z"),
            coverage_complete: false,
            fingerprint: digest_fingerprint(9),
            project_breakdown_fingerprint: digest_fingerprint(10),
            event_count: 1,
            metric_revision: nonzero32(3),
            estimator_revision: nonzero32(4),
            project_breakdown_revision: nonzero32(5),
            api_pricing_catalog_revision: nonzero32(6),
        }
    }

    fn remote_tokens() -> RemoteTokenUsage {
        RemoteTokenUsage {
            input_tokens: 100,
            cached_input_tokens: 20,
            cache_write_input_tokens: 5,
            output_tokens: 10,
            reasoning_output_tokens: 4,
            total_tokens: 110,
        }
    }

    fn remote_api_cost() -> RemoteApiCostAmount {
        RemoteApiCostAmount {
            minimum_pico_usd: RemoteU128::new(1_000),
            maximum_pico_usd: RemoteU128::new(1_500),
            observed_samples: 1,
            priced_samples: 1,
            observed_tokens: 110,
            priced_tokens: 110,
        }
    }

    #[test]
    fn api_cost_population_can_exceed_codex_token_population_for_spark() {
        let api_cost = RemoteApiCostAmount {
            minimum_pico_usd: RemoteU128::new(0),
            maximum_pico_usd: RemoteU128::new(0),
            observed_samples: 1,
            priced_samples: 0,
            observed_tokens: 110,
            priced_tokens: 0,
        };

        api_cost.validate(RemoteTokenUsage::default()).unwrap();
    }

    fn delta_request(include_live: bool) -> RemoteExportRequest {
        RemoteExportRequest {
            request: RemoteExportRequestBody::Delta(DeltaRequest {
                delta_cursor: Some(DeltaCursor {
                    generation: nonzero64(9),
                    sequence: 10,
                }),
                range: ExportRange {
                    from: at("2026-08-30T00:00:00Z"),
                    to: at("2026-08-30T02:00:00Z"),
                },
                overlap_minutes: 60,
                include_live,
                known_live_revision: None,
            }),
            ..probe_request()
        }
    }

    fn delta_payload(include_live: bool) -> DeltaPayload {
        let project_key = observed_project_key(1);
        let bucket = RemoteUsageBucket {
            starts_at: at("2026-08-30T01:00:00Z"),
            ends_at: at("2026-08-30T01:15:00Z"),
            sampled_at: at("2026-08-30T01:02:00Z"),
            token_usage: remote_tokens(),
            estimated_cost_units: RemoteU128::new(75),
            api_long_context_extra_cost_units: Some(RemoteU128::new(25)),
            long_context_usage_unknown: false,
            api_equivalent_cost: remote_api_cost(),
            call_count: 1,
            metric_revision: nonzero32(3),
            estimator_revision: nonzero32(4),
            project_breakdown_revision: nonzero32(5),
            api_pricing_catalog_revision: nonzero32(6),
            model_groups: vec![RemoteModelUsageGroup {
                model: Some("gpt-5.6-sol".to_owned()),
                service_tier: Some("standard".to_owned()),
                token_usage: remote_tokens(),
                estimated_cost_units: RemoteU128::new(75),
                api_long_context_extra_cost_units: Some(RemoteU128::new(25)),
                api_equivalent_cost: remote_api_cost(),
                call_count: 1,
                used_model_fallback: false,
                used_token_breakdown_fallback: false,
                used_long_context_pricing: true,
                used_long_context_detection_fallback: false,
            }],
            project_groups: vec![RemoteProjectUsageGroup {
                observed_project_key: Some(project_key.clone()),
                emitting_thread_id: "thread-1".parse().unwrap(),
                emitting_turn_id: Some("turn-1".to_owned()),
                parent_thread_id: None,
                root_session_thread_id: Some("thread-1".parse().unwrap()),
                root_session_turn_id: Some("turn-1".to_owned()),
                title_preview: None,
                message_preview: None,
                token_usage: remote_tokens(),
                estimated_cost_units: RemoteU128::new(75),
                api_long_context_extra_cost_units: Some(RemoteU128::new(25)),
                api_equivalent_cost: remote_api_cost(),
                call_count: 1,
            }],
            partial_reasons: Vec::new(),
        };
        let digest = RemoteSessionDigest {
            thread_id: "thread-1".parse().unwrap(),
            range_start: at("2026-08-30T00:00:00Z"),
            range_end: at("2026-08-30T01:15:00Z"),
            covered_through: at("2026-08-30T01:02:00Z"),
            fingerprint: digest_fingerprint(2),
            project_breakdown_fingerprint: digest_fingerprint(3),
            event_count: 1,
            exact_event_identity: true,
            coverage_complete: false,
            observed_project_keys: vec![project_key.clone()],
            metrics: RemoteSessionUsageMetrics {
                token_usage: remote_tokens(),
                estimated_cost_units: RemoteU128::new(75),
                api_long_context_extra_cost_units: Some(RemoteU128::new(25)),
                api_equivalent_cost: remote_api_cost(),
                call_count: 1,
                metric_revision: nonzero32(3),
                estimator_revision: nonzero32(4),
                project_breakdown_revision: nonzero32(5),
                api_pricing_catalog_revision: nonzero32(6),
                partial_reasons: vec!["open_session".to_owned()],
            },
        };
        let live = include_live.then(|| RemoteLiveState {
            live_revision: nonzero64(4),
            snapshot: Some(RemoteLiveSnapshot {
                captured_at: at("2026-08-30T01:02:00Z"),
                tasks: vec![RemoteLiveTask {
                    thread_id: "thread-1".parse().unwrap(),
                    parent_thread_id: None,
                    observed_project_key: Some(project_key.clone()),
                    title_preview: None,
                    created_at: Some(at("2026-08-30T00:00:00Z")),
                    updated_at: at("2026-08-30T01:02:00Z"),
                    status: TaskStatus::Running,
                    token_usage: remote_tokens(),
                    turn_count: 1,
                }],
                turns: vec![RemoteLiveTurn {
                    thread_id: "thread-1".parse().unwrap(),
                    turn_id: "turn-1".to_owned(),
                    model: Some("gpt-5.6-sol".to_owned()),
                    reasoning_effort: Some("high".to_owned()),
                    service_tier: Some("standard".to_owned()),
                    message_preview: None,
                    started_at: Some(at("2026-08-30T01:00:00Z")),
                    completed_at: None,
                    status: TurnStatus::InProgress,
                    token_usage: remote_tokens(),
                }],
            }),
        });
        DeltaPayload {
            coverage: RemoteDeltaCoverage {
                requested_range: ExportRange {
                    from: at("2026-08-30T00:00:00Z"),
                    to: at("2026-08-30T02:00:00Z"),
                },
                covered_range: Some(ExportRange {
                    from: at("2026-08-30T00:00:00Z"),
                    to: at("2026-08-30T01:15:00Z"),
                }),
                range_complete: false,
                partial_reasons: vec!["backfill_incomplete".to_owned()],
            },
            project_descriptors: vec![RemoteProjectDescriptor {
                observed_project_key: project_key,
                display_label: "workspace".parse().unwrap(),
                git_evidence: RemoteGitRepositoryEvidence::Repository {
                    fingerprint: Some(git_fingerprint(3)),
                    repository_relative_workspace_root: "crates/core".to_owned(),
                },
            }],
            bucket_changes: vec![RemoteUsageBucketChange {
                sequence: nonzero64(11),
                starts_at: at("2026-08-30T01:00:00Z"),
                revision: nonzero64(2),
                mutation: RemoteUsageBucketMutation::Upsert(Box::new(bucket)),
            }],
            session_digest_changes: vec![RemoteSessionDigestChange {
                sequence: nonzero64(12),
                thread_id: "thread-1".parse().unwrap(),
                range_start: at("2026-08-30T00:00:00Z"),
                range_end: at("2026-08-30T01:15:00Z"),
                changed_at: at("2026-08-30T01:02:00Z"),
                retention_through: at("2026-08-30T01:15:00Z"),
                revision: nonzero64(3),
                mutation: RemoteSessionDigestMutation::Upsert(Box::new(digest)),
            }],
            live,
            stats: RemoteDeltaStats {
                journal_records_scanned: 2,
                project_descriptors_emitted: 1,
                bucket_changes_emitted: 1,
                session_digest_changes_emitted: 1,
                live_tasks_emitted: u64::from(include_live),
                live_turns_emitted: u64::from(include_live),
                discovered_files: 2,
                parsed_files: 1,
                reused_files: 1,
                unreadable_files: 0,
                truncated_files: 0,
                skipped_lines: 0,
                ambiguous_token_resets: 0,
                warnings_suppressed: 0,
                ..RemoteDeltaStats::default()
            },
            warnings: vec![RemoteDeltaWarning {
                code: "backfill_incomplete".to_owned(),
                occurrences: nonzero64(1),
            }],
        }
    }

    fn delta_response(payload: DeltaPayload) -> RemoteDeltaResponse {
        let received = at("2026-08-30T01:02:03Z");
        RemoteExportResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: "0.4.0-test".parse().unwrap(),
            source: source(),
            redaction_profile: RedactionProfile::Redacted,
            revisions: revisions(),
            observed_at: received + Duration::seconds(1),
            timing: RemoteTiming {
                remote_received_at: received,
                remote_sent_at: received + Duration::seconds(2),
            },
            result: RemoteExportResponseBody::Delta {
                page: DeltaPage {
                    generation: nonzero64(9),
                    from_sequence: 11,
                    through_sequence: 12,
                    next_delta_cursor: DeltaCursor {
                        generation: nonzero64(9),
                        sequence: 12,
                    },
                    has_more: false,
                },
                payload,
            },
        }
    }

    fn probe_request() -> RemoteExportRequest {
        RemoteExportRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            client_version: "0.4.0-test".parse().unwrap(),
            expected_source: Some(source()),
            redaction_profile: RedactionProfile::Redacted,
            max_page_bytes: MAX_REMOTE_FRAME_ENCODED_BYTES as u32,
            accepted_revisions: accepted_revisions(),
            request: RemoteExportRequestBody::Probe(ProbeRequest {
                check_state_writable: true,
                check_rollout_readable: true,
            }),
        }
    }

    fn response_with(
        result: RemoteExportResponseBody<EmptyRemotePayload, EmptyRemotePayload>,
    ) -> RemoteExportResponse {
        let received = DateTime::parse_from_rfc3339("2026-08-30T01:02:03Z")
            .unwrap()
            .with_timezone(&Utc);
        RemoteExportResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: "0.4.0-test".parse().unwrap(),
            source: source(),
            redaction_profile: RedactionProfile::Redacted,
            revisions: revisions(),
            observed_at: received + Duration::seconds(1),
            timing: RemoteTiming {
                remote_received_at: received,
                remote_sent_at: received + Duration::seconds(2),
            },
            result,
        }
    }

    fn fact_metrics() -> RemoteSessionUsageMetrics {
        RemoteSessionUsageMetrics {
            token_usage: remote_tokens(),
            estimated_cost_units: RemoteU128::new(42),
            api_long_context_extra_cost_units: Some(RemoteU128::new(0)),
            api_equivalent_cost: remote_api_cost(),
            call_count: 1,
            metric_revision: revisions().metric,
            estimator_revision: revisions().estimator,
            project_breakdown_revision: revisions().project_breakdown,
            api_pricing_catalog_revision: revisions().api_pricing_catalog,
            partial_reasons: vec!["usage_event_identity_fallback".to_owned()],
        }
    }

    fn fact() -> RemoteUsageEventFact {
        RemoteUsageEventFact {
            event_id: "usage-derived-sha256-v1-0123456789abcdef".parse().unwrap(),
            occurred_at: at("2026-08-30T01:00:00Z"),
            observed_project_key: observed_project_key(1),
            emitting_thread_id: "thread-a".parse().unwrap(),
            emitting_turn_id: Some("turn-1".to_owned()),
            parent_thread_id: None,
            project_session_thread_id: Some("thread-a".parse().unwrap()),
            root_session_thread_id: "thread-a".parse().unwrap(),
            root_session_turn_id: Some("turn-1".to_owned()),
            model: Some("gpt-5.6-sol".to_owned()),
            service_tier: None,
            digest_token_usage: remote_tokens(),
            request_usage_exact: true,
            exact_event_identity: false,
            metrics: fact_metrics(),
        }
    }

    fn fact_response<F: RemotePagePayload>(
        result: RemoteExportResponseBody<EmptyRemotePayload, F>,
    ) -> RemoteExportResponse<EmptyRemotePayload, F> {
        let received = at("2026-08-30T01:02:03Z");
        RemoteExportResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            server_version: "0.4.0-test".parse().unwrap(),
            source: source(),
            redaction_profile: RedactionProfile::Redacted,
            revisions: revisions(),
            observed_at: received + Duration::seconds(1),
            timing: RemoteTiming {
                remote_received_at: received,
                remote_sent_at: received + Duration::seconds(2),
            },
            result,
        }
    }

    fn raw_identity_frame(json: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + json.len());
        frame.extend_from_slice(&FRAME_MAGIC);
        frame.push(FRAME_VERSION);
        frame.push(ENCODING_IDENTITY);
        frame.extend_from_slice(&0_u16.to_be_bytes());
        frame.extend_from_slice(&(json.len() as u32).to_be_bytes());
        frame.extend_from_slice(&(json.len() as u32).to_be_bytes());
        frame.extend_from_slice(json);
        frame
    }

    #[test]
    fn probe_request_round_trips_as_one_identity_frame() {
        let request = probe_request();
        let frame = encode_remote_frame(&request, RemoteFrameLimits::default()).unwrap();
        assert_eq!(frame[9], ENCODING_IDENTITY);
        let decoded: RemoteExportRequest =
            decode_remote_frame(&frame, RemoteFrameLimits::default()).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            decoded_remote_frame_payload_len(&frame).unwrap(),
            serde_json::to_vec(&request).unwrap().len()
        );
        assert!(matches!(
            decoded_remote_frame_payload_len(&frame[..FRAME_HEADER_BYTES - 1]),
            Err(error) if error.kind() == RemoteProtocolErrorKind::TruncatedFrame
        ));

        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["request"]["requestKind"], "probe");
        assert!(value["request"].get("delta").is_none());
        assert!(value["request"].get("sessionFacts").is_none());
    }

    #[test]
    fn request_kinds_are_schema_mutually_exclusive() {
        let mut value = serde_json::to_value(probe_request()).unwrap();
        value["request"]["parameters"]["deltaCursor"] = json!(null);
        let frame = raw_identity_frame(&serde_json::to_vec(&value).unwrap());
        let error =
            decode_remote_frame::<RemoteExportRequest>(&frame, RemoteFrameLimits::default())
                .unwrap_err();
        assert_eq!(error.kind(), RemoteProtocolErrorKind::InvalidJson);
    }

    #[test]
    fn unknown_request_fields_fail_closed() {
        let mut value = serde_json::to_value(probe_request()).unwrap();
        value["unexpected"] = json!(true);
        let frame = raw_identity_frame(&serde_json::to_vec(&value).unwrap());
        let error =
            decode_remote_frame::<RemoteExportRequest>(&frame, RemoteFrameLimits::default())
                .unwrap_err();
        assert_eq!(error.kind(), RemoteProtocolErrorKind::InvalidJson);
    }

    #[test]
    fn request_semantics_validate_revisions_ranges_and_retention() {
        let mut request = probe_request();
        request.accepted_revisions.metric.min = nonzero32(9);
        request.accepted_revisions.metric.max = nonzero32(8);
        assert_eq!(
            encode_remote_frame(&request, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        request = probe_request();
        request.max_page_bytes = (MIN_REMOTE_RESPONSE_ENCODED_BYTES - 1) as u32;
        assert_eq!(
            encode_remote_frame(&request, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        request = probe_request();
        request.request = RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
            thread_id: "01abcdef".parse().unwrap(),
            retention_days: 36,
            expected_digests: vec![fact_digest_binding()],
            position: SessionFactsPosition::SnapshotStart,
        });
        assert_eq!(
            encode_remote_frame(&request, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );
    }

    #[test]
    fn data_requests_require_a_pinned_source_identity() {
        let now = Utc::now();
        let mut request = probe_request();
        request.expected_source = None;
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
        assert_eq!(
            encode_remote_frame(&request, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        request.request = RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
            thread_id: "01abcdef".parse().unwrap(),
            retention_days: 35,
            expected_digests: vec![fact_digest_binding()],
            position: SessionFactsPosition::SnapshotStart,
        });
        assert_eq!(
            encode_remote_frame(&request, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );
    }

    #[test]
    fn delta_and_fact_cursor_domains_have_distinct_wire_fields() {
        let delta = serde_json::to_value(DeltaCursor {
            generation: nonzero64(2),
            sequence: 10,
        })
        .unwrap();
        let fact = serde_json::to_value(FactCursor {
            fact_generation: nonzero64(2),
            through_sequence: 10,
        })
        .unwrap();
        assert!(delta.get("generation").is_some());
        assert!(delta.get("factGeneration").is_none());
        assert!(fact.get("factGeneration").is_some());
        assert!(fact.get("generation").is_none());

        assert!(serde_json::from_value::<FactCursor>(delta).is_err());
        assert!(serde_json::from_value::<DeltaCursor>(fact).is_err());
    }

    #[test]
    fn fact_snapshot_and_delta_page_tokens_are_distinct() {
        let position = SessionFactsPosition::SnapshotContinue {
            snapshot_id: "snapshot-1".parse().unwrap(),
            fact_generation: nonzero64(2),
            snapshot_watermark: 10,
            page_token: "snapshot.token-1".parse().unwrap(),
        };
        let value = serde_json::to_value(position).unwrap();
        assert_eq!(value["positionKind"], "snapshotContinue");
        assert!(
            serde_json::from_value::<SessionFactsPosition>(json!({
                "positionKind": "snapshotContinue",
                "position": {
                    "pageToken": "bad token with spaces"
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn accepted_revision_negotiation_is_domain_specific() {
        let accepted = accepted_revisions();
        assert!(accepted.accepts(&revisions()));
        let mut incompatible = revisions();
        incompatible.api_pricing_catalog = nonzero32(7);
        assert!(!accepted.accepts(&incompatible));
    }

    #[test]
    fn response_negotiation_checks_identity_redaction_revisions_and_kind() {
        let request = probe_request();
        let valid = response_with(RemoteExportResponseBody::Probe(ProbeResult {
            capabilities: vec![RemoteCapability::GzipFrame],
            state_writable: true,
            rollout_readable: true,
        }));
        valid.validate_for_request(&request).unwrap();

        let mut wrong_identity = valid.clone();
        wrong_identity.source.generation = nonzero64(8);
        assert!(wrong_identity.validate_for_request(&request).is_err());

        let mut wrong_redaction = valid.clone();
        wrong_redaction.redaction_profile = RedactionProfile::PreviewEnabled;
        assert!(wrong_redaction.validate_for_request(&request).is_err());

        let mut wrong_revision = valid.clone();
        wrong_revision.revisions.metric = nonzero32(99);
        assert!(wrong_revision.validate_for_request(&request).is_err());

        let wrong_kind = response_with(RemoteExportResponseBody::Delta {
            page: DeltaPage {
                generation: nonzero64(1),
                from_sequence: 0,
                through_sequence: 0,
                next_delta_cursor: DeltaCursor {
                    generation: nonzero64(1),
                    sequence: 0,
                },
                has_more: false,
            },
            payload: EmptyRemotePayload {},
        });
        assert!(wrong_kind.validate_for_request(&request).is_err());

        let failure = response_with(RemoteExportResponseBody::Failure(RemoteFailure {
            kind: RemoteFailureKind::IdentityMismatch,
            message: "source identity changed".to_owned(),
            retry_after_seconds: None,
        }));
        failure.validate_for_request(&request).unwrap();
    }

    #[test]
    fn response_pages_must_continue_the_exact_request_context() {
        let mut request = probe_request();
        request.request = RemoteExportRequestBody::Delta(DeltaRequest {
            delta_cursor: Some(DeltaCursor {
                generation: nonzero64(3),
                sequence: 10,
            }),
            range: ExportRange {
                from: Utc::now() - Duration::hours(1),
                to: Utc::now(),
            },
            overlap_minutes: 60,
            include_live: true,
            known_live_revision: None,
        });
        let valid_delta = response_with(RemoteExportResponseBody::Delta {
            page: DeltaPage {
                generation: nonzero64(3),
                from_sequence: 11,
                through_sequence: 12,
                next_delta_cursor: DeltaCursor {
                    generation: nonzero64(3),
                    sequence: 12,
                },
                has_more: false,
            },
            payload: EmptyRemotePayload {},
        });
        valid_delta.validate_for_request(&request).unwrap();
        let mut wrong_generation = valid_delta.clone();
        let RemoteExportResponseBody::Delta { page, .. } = &mut wrong_generation.result else {
            unreachable!();
        };
        page.generation = nonzero64(4);
        page.next_delta_cursor.generation = nonzero64(4);
        assert!(wrong_generation.validate_for_request(&request).is_err());

        request.request = RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
            thread_id: "01abcdef".parse().unwrap(),
            retention_days: 35,
            expected_digests: vec![fact_digest_binding()],
            position: SessionFactsPosition::SnapshotContinue {
                snapshot_id: "snapshot-1".parse().unwrap(),
                fact_generation: nonzero64(5),
                snapshot_watermark: 20,
                page_token: "page-2".parse().unwrap(),
            },
        });
        let valid_snapshot = response_with(RemoteExportResponseBody::FactSnapshot {
            page: FactSnapshotPage {
                thread_id: "01abcdef".parse().unwrap(),
                snapshot_id: "snapshot-1".parse().unwrap(),
                fact_generation: nonzero64(5),
                snapshot_watermark: 20,
                next_page_token: None,
                activate_fact_cursor: Some(FactCursor {
                    fact_generation: nonzero64(5),
                    through_sequence: 20,
                }),
                has_more: false,
            },
            payload: EmptyRemotePayload {},
        });
        valid_snapshot.validate_for_request(&request).unwrap();
        let mut wrong_snapshot = valid_snapshot.clone();
        let RemoteExportResponseBody::FactSnapshot { page, .. } = &mut wrong_snapshot.result else {
            unreachable!();
        };
        page.snapshot_id = "snapshot-2".parse().unwrap();
        assert!(wrong_snapshot.validate_for_request(&request).is_err());
    }

    #[test]
    fn typed_delta_payload_round_trips_with_exact_wire_numbers_and_context() {
        let request = delta_request(true);
        let response = delta_response(delta_payload(true));
        let frame =
            encode_remote_response_for_request(&response, &request, RemoteFrameLimits::default())
                .unwrap();
        let decoded: RemoteDeltaResponse =
            decode_remote_response_for_request(&frame, &request, RemoteFrameLimits::default())
                .unwrap();
        assert_eq!(decoded, response);

        let value = serde_json::to_value(&decoded).unwrap();
        let payload = &value["result"]["response"]["payload"];
        assert_eq!(
            payload["bucketChanges"][0]["mutation"]["value"]["estimatedCostUnits"],
            "75"
        );
        assert_eq!(
            payload["bucketChanges"][0]["mutation"]["value"]["apiEquivalentCost"]["maximumPicoUsd"],
            "1500"
        );
        assert!(value.to_string().find("/Users/").is_none());
        assert!(value.to_string().find("https://").is_none());
    }

    #[test]
    fn typed_delta_rejects_order_identity_descriptor_and_revision_inconsistency() {
        let request = delta_request(true);

        let mut missing_descriptor = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut missing_descriptor.result else {
            unreachable!();
        };
        payload.project_descriptors.clear();
        payload.stats.project_descriptors_emitted = 0;
        assert_eq!(
            encode_remote_response_for_request(
                &missing_descriptor,
                &request,
                RemoteFrameLimits::default(),
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        // A hostile peer must not be able to consume the full descriptor
        // allowance with metadata that no fact on this page can reference.
        // Reject this before serialization and before center-side mapping
        // capacity can be touched.
        let mut descriptor_capacity_attack = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } =
            &mut descriptor_capacity_attack.result
        else {
            unreachable!()
        };
        payload.project_descriptors = (1..=MAX_PROJECT_DESCRIPTORS_PER_PAGE)
            .map(|index| RemoteProjectDescriptor {
                observed_project_key: observed_project_key(index as u64),
                display_label: format!("unused-{index:05}").parse().unwrap(),
                git_evidence: RemoteGitRepositoryEvidence::Unavailable,
            })
            .collect();
        payload.stats.project_descriptors_emitted = MAX_PROJECT_DESCRIPTORS_PER_PAGE as u64;
        assert_eq!(
            encode_remote_response_for_request(
                &descriptor_capacity_attack,
                &request,
                RemoteFrameLimits::default(),
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        let mut duplicate_sequence = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut duplicate_sequence.result else {
            unreachable!();
        };
        payload.session_digest_changes[0].sequence = nonzero64(11);
        assert_eq!(
            encode_remote_response_for_request(
                &duplicate_sequence,
                &request,
                RemoteFrameLimits::default(),
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        let mut wrong_revision = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut wrong_revision.result else {
            unreachable!();
        };
        let RemoteUsageBucketMutation::Upsert(bucket) = &mut payload.bucket_changes[0].mutation
        else {
            unreachable!();
        };
        bucket.metric_revision = nonzero32(99);
        assert_eq!(
            encode_remote_response_for_request(
                &wrong_revision,
                &request,
                RemoteFrameLimits::default(),
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        let mut wrong_key = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut wrong_key.result else {
            unreachable!();
        };
        payload.bucket_changes[0].starts_at = at("2026-08-30T01:15:00Z");
        assert_eq!(
            encode_remote_response_for_request(&wrong_key, &request, RemoteFrameLimits::default(),)
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );
    }

    #[test]
    fn center_rejects_live_rows_over_the_source_serialized_budget() {
        let request = delta_request(true);
        let mut response = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut response.result else {
            unreachable!()
        };
        let snapshot = payload
            .live
            .as_mut()
            .and_then(|live| live.snapshot.as_mut())
            .unwrap();
        snapshot.tasks[0].turn_count = MAX_LIVE_TURNS as u32;
        snapshot.turns = (0..MAX_LIVE_TURNS)
            .map(|index| RemoteLiveTurn {
                thread_id: "thread-1".parse().unwrap(),
                turn_id: format!("turn-{index:04}-{}", "x".repeat(230)),
                model: Some("m".repeat(240)),
                reasoning_effort: Some("high".to_owned()),
                service_tier: Some("standard".to_owned()),
                message_preview: None,
                started_at: Some(at("2026-08-30T01:00:00Z")),
                completed_at: None,
                status: TurnStatus::InProgress,
                token_usage: remote_tokens(),
            })
            .collect();
        payload.stats.live_turns_emitted = MAX_LIVE_TURNS as u64;

        // Build a raw peer frame so source-side encoding validation cannot
        // mask the center's independent trust-boundary check.
        let frame = raw_identity_frame(&serde_json::to_vec(&response).unwrap());
        let error = decode_remote_response_for_request::<DeltaPayload, EmptyRemotePayload>(
            &frame,
            &request,
            RemoteFrameLimits::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), RemoteProtocolErrorKind::InvalidMessage);
        assert!(error.to_string().contains("serialized byte bound"));
    }

    #[test]
    fn typed_delta_binds_live_redaction_range_stats_and_cursor_progress() {
        let request = delta_request(true);

        let mut preview_leak = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut preview_leak.result else {
            unreachable!();
        };
        payload
            .live
            .as_mut()
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .tasks[0]
            .title_preview = Some("secret".to_owned());
        assert!(
            encode_remote_response_for_request(
                &preview_leak,
                &request,
                RemoteFrameLimits::default(),
            )
            .is_err()
        );

        let mut orphan_turn = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut orphan_turn.result else {
            unreachable!();
        };
        payload
            .live
            .as_mut()
            .unwrap()
            .snapshot
            .as_mut()
            .unwrap()
            .turns[0]
            .thread_id = "missing-thread".parse().unwrap();
        assert!(
            encode_remote_response_for_request(
                &orphan_turn,
                &request,
                RemoteFrameLimits::default(),
            )
            .is_err(),
            "a live turn without its task would be invisible in the center TUI"
        );

        let mut missing_live = delta_response(delta_payload(false));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut missing_live.result else {
            unreachable!();
        };
        payload.stats.live_tasks_emitted = 0;
        payload.stats.live_turns_emitted = 0;
        assert!(
            encode_remote_response_for_request(
                &missing_live,
                &request,
                RemoteFrameLimits::default(),
            )
            .is_err()
        );

        let mut wrong_range = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut wrong_range.result else {
            unreachable!();
        };
        payload.coverage.requested_range.from = at("2026-08-30T00:15:00Z");
        assert!(
            encode_remote_response_for_request(
                &wrong_range,
                &request,
                RemoteFrameLimits::default(),
            )
            .is_err()
        );

        let mut wrong_scan_count = delta_response(delta_payload(true));
        let RemoteExportResponseBody::Delta { payload, .. } = &mut wrong_scan_count.result else {
            unreachable!();
        };
        payload.stats.journal_records_scanned = 1;
        assert!(
            encode_remote_response_for_request(
                &wrong_scan_count,
                &request,
                RemoteFrameLimits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn global_delta_cursor_can_carry_transitions_outside_the_coverage_range() {
        let mut request = delta_request(true);
        let RemoteExportRequestBody::Delta(delta_request) = &mut request.request else {
            unreachable!();
        };
        delta_request.range = ExportRange {
            from: at("2026-08-31T00:00:00Z"),
            to: at("2026-08-31T02:00:00Z"),
        };

        let mut payload = delta_payload(true);
        payload.coverage = RemoteDeltaCoverage {
            requested_range: delta_request.range.clone(),
            covered_range: None,
            range_complete: false,
            partial_reasons: vec!["historical_coverage_unproven".to_owned()],
        };
        let response = delta_response(payload);

        encode_remote_response_for_request(&response, &request, RemoteFrameLimits::default())
            .unwrap();
    }

    #[test]
    fn typed_session_fact_snapshot_and_delta_round_trip_and_fail_closed() {
        let mut snapshot_request = probe_request();
        snapshot_request.request = RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
            thread_id: "thread-a".parse().unwrap(),
            retention_days: 35,
            expected_digests: vec![fact_digest_binding()],
            position: SessionFactsPosition::SnapshotStart,
        });
        let snapshot_record = RemoteUsageEventFactRecord {
            event_id: fact().event_id.clone(),
            occurred_at: fact().occurred_at,
            revision: nonzero64(4),
            mutation: RemoteUsageEventFactMutation::Upsert(Box::new(fact())),
        };
        let snapshot_response = fact_response(RemoteExportResponseBody::FactSnapshot {
            page: FactSnapshotPage {
                thread_id: "thread-a".parse().unwrap(),
                snapshot_id: "fact-snapshot-1".parse().unwrap(),
                fact_generation: nonzero64(9),
                snapshot_watermark: 4,
                next_page_token: None,
                activate_fact_cursor: Some(FactCursor {
                    fact_generation: nonzero64(9),
                    through_sequence: 4,
                }),
                has_more: false,
            },
            payload: RemoteSessionFactPayload::Snapshot(RemoteFactSnapshotPayload {
                fact_schema_version: REMOTE_SESSION_FACT_SCHEMA_VERSION,
                records: vec![snapshot_record.clone()],
            }),
        });
        let frame = encode_remote_response_for_request(
            &snapshot_response,
            &snapshot_request,
            RemoteFrameLimits::default(),
        )
        .unwrap();
        decode_remote_response_for_request::<EmptyRemotePayload, RemoteSessionFactPayload>(
            &frame,
            &snapshot_request,
            RemoteFrameLimits::default(),
        )
        .unwrap();

        let mut snapshot_tombstone = snapshot_response.clone();
        let RemoteExportResponseBody::FactSnapshot { payload, .. } = &mut snapshot_tombstone.result
        else {
            unreachable!();
        };
        let RemoteSessionFactPayload::Snapshot(payload) = payload else {
            unreachable!();
        };
        payload.records[0].mutation = RemoteUsageEventFactMutation::Tombstone;
        assert_eq!(
            encode_remote_response_for_request(
                &snapshot_tombstone,
                &snapshot_request,
                RemoteFrameLimits::default(),
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        let cursor = FactCursor {
            fact_generation: nonzero64(9),
            through_sequence: 4,
        };
        let mut delta_request = snapshot_request.clone();
        delta_request.request = RemoteExportRequestBody::SessionFacts(SessionFactsRequest {
            thread_id: "thread-a".parse().unwrap(),
            retention_days: 35,
            expected_digests: vec![fact_digest_binding()],
            position: SessionFactsPosition::DeltaStart {
                fact_cursor: cursor,
            },
        });
        let delta_response = fact_response(RemoteExportResponseBody::FactDelta {
            page: FactDeltaPage {
                thread_id: "thread-a".parse().unwrap(),
                batch_id: "fact-delta-1".parse().unwrap(),
                fact_generation: nonzero64(9),
                delta_watermark: 5,
                next_page_token: None,
                activate_fact_cursor: Some(FactCursor {
                    fact_generation: nonzero64(9),
                    through_sequence: 5,
                }),
                has_more: false,
            },
            payload: RemoteSessionFactPayload::Delta(RemoteFactDeltaPayload {
                fact_schema_version: REMOTE_SESSION_FACT_SCHEMA_VERSION,
                changes: vec![RemoteUsageEventFactDeltaChange {
                    sequence: nonzero64(5),
                    record: RemoteUsageEventFactRecord {
                        revision: nonzero64(5),
                        ..snapshot_record
                    },
                }],
            }),
        });
        let frame = encode_remote_response_for_request(
            &delta_response,
            &delta_request,
            RemoteFrameLimits::default(),
        )
        .unwrap();
        decode_remote_response_for_request::<EmptyRemotePayload, RemoteSessionFactPayload>(
            &frame,
            &delta_request,
            RemoteFrameLimits::default(),
        )
        .unwrap();

        let mut wrong_delta_revision = delta_response;
        let RemoteExportResponseBody::FactDelta { payload, .. } = &mut wrong_delta_revision.result
        else {
            unreachable!();
        };
        let RemoteSessionFactPayload::Delta(payload) = payload else {
            unreachable!();
        };
        payload.changes[0].record.revision = nonzero64(4);
        assert_eq!(
            encode_remote_response_for_request(
                &wrong_delta_revision,
                &delta_request,
                RemoteFrameLimits::default(),
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );
    }

    #[test]
    fn typed_delta_nested_schema_counts_and_decimal_ids_fail_closed() {
        let request = delta_request(true);
        let response = delta_response(delta_payload(true));
        let mut value = serde_json::to_value(&response).unwrap();
        value["result"]["response"]["payload"]["bucketChanges"][0]["mutation"]["value"]["tokenUsage"]
            ["unexpected"] = json!(1);
        let frame = raw_identity_frame(&serde_json::to_vec(&value).unwrap());
        assert_eq!(
            decode_remote_response_for_request::<DeltaPayload, EmptyRemotePayload>(
                &frame,
                &request,
                RemoteFrameLimits::default(),
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::InvalidJson
        );

        assert!(serde_json::from_value::<RemoteU128>(json!("01")).is_err());
        assert!(serde_json::from_value::<RemoteU128>(json!(1)).is_err());
        assert!(
            "git-sha256-v1-ABC"
                .parse::<GitRepositoryFingerprint>()
                .is_err()
        );
        assert!(
            "session-digest-sha256-v1-00"
                .parse::<RemoteSessionDigestFingerprint>()
                .is_err()
        );
        assert!(
            validate_count(
                MAX_BUCKET_CHANGES_PER_PAGE + 1,
                MAX_BUCKET_CHANGES_PER_PAGE,
                "test changes",
            )
            .is_err()
        );
    }

    #[test]
    fn negotiated_delta_caps_apply_before_decode_and_large_pages_use_gzip() {
        let mut request = delta_request(false);
        let mut payload = delta_payload(false);
        payload.bucket_changes.clear();
        let mut digest_change = payload.session_digest_changes.pop().unwrap();
        payload.warnings.clear();
        payload.coverage = RemoteDeltaCoverage {
            requested_range: match &request.request {
                RemoteExportRequestBody::Delta(delta) => delta.range.clone(),
                _ => unreachable!(),
            },
            covered_range: Some(match &request.request {
                RemoteExportRequestBody::Delta(delta) => delta.range.clone(),
                _ => unreachable!(),
            }),
            range_complete: true,
            partial_reasons: Vec::new(),
        };
        payload.project_descriptors = (1..=2_000)
            .map(|index| RemoteProjectDescriptor {
                observed_project_key: observed_project_key(index),
                display_label: format!("workspace-{index:04}").parse().unwrap(),
                git_evidence: RemoteGitRepositoryEvidence::Repository {
                    fingerprint: Some(git_fingerprint(index)),
                    repository_relative_workspace_root: format!("crates/{index:04}"),
                },
            })
            .collect();
        let RemoteSessionDigestMutation::Upsert(digest) = &mut digest_change.mutation else {
            unreachable!()
        };
        digest.observed_project_keys = payload
            .project_descriptors
            .iter()
            .map(|descriptor| descriptor.observed_project_key.clone())
            .collect();
        digest_change.sequence = nonzero64(11);
        payload.session_digest_changes = vec![digest_change];
        payload.stats = RemoteDeltaStats {
            journal_records_scanned: 1,
            project_descriptors_emitted: payload.project_descriptors.len() as u64,
            session_digest_changes_emitted: 1,
            ..RemoteDeltaStats::default()
        };
        let mut response = delta_response(payload);
        let RemoteExportResponseBody::Delta { page, .. } = &mut response.result else {
            unreachable!();
        };
        page.from_sequence = 11;
        page.through_sequence = 11;
        page.next_delta_cursor.sequence = 11;

        let frame =
            encode_remote_response_for_request(&response, &request, RemoteFrameLimits::default())
                .unwrap();
        assert_eq!(frame[9], ENCODING_GZIP);
        decode_remote_response_for_request::<DeltaPayload, EmptyRemotePayload>(
            &frame,
            &request,
            RemoteFrameLimits::default(),
        )
        .unwrap();

        request.max_page_bytes = (frame.len() - FRAME_HEADER_BYTES - 1) as u32;
        assert_eq!(
            decode_remote_response_for_request::<DeltaPayload, EmptyRemotePayload>(
                &frame,
                &request,
                RemoteFrameLimits::default(),
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::EncodedLimitExceeded
        );

        let mut decoded_limited_request = request.clone();
        decoded_limited_request.max_page_bytes = MAX_REMOTE_FRAME_ENCODED_BYTES as u32;
        assert_eq!(
            encode_remote_response_for_request(
                &response,
                &decoded_limited_request,
                RemoteFrameLimits {
                    max_encoded_bytes: MAX_REMOTE_FRAME_ENCODED_BYTES,
                    max_decoded_bytes: 1_024,
                    identity_threshold_bytes: 1_024,
                },
            )
            .unwrap_err()
            .kind(),
            RemoteProtocolErrorKind::DecodedLimitExceeded
        );
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct LargeMessage {
        text: String,
    }

    impl RemoteProtocolMessage for LargeMessage {
        fn validate_remote_protocol(&self) -> Result<(), RemoteProtocolError> {
            Ok(())
        }
    }

    #[test]
    fn encoding_stops_at_the_decoded_limit_without_an_unbounded_json_buffer() {
        let limits = RemoteFrameLimits {
            max_encoded_bytes: 1024,
            max_decoded_bytes: 1024,
            identity_threshold_bytes: 1024,
        };
        let error = encode_remote_frame(
            &LargeMessage {
                text: "x".repeat(2048),
            },
            limits,
        )
        .unwrap_err();
        assert_eq!(error.kind(), RemoteProtocolErrorKind::DecodedLimitExceeded);
    }

    #[test]
    fn large_message_uses_fast_gzip_and_round_trips() {
        let message = LargeMessage {
            text: "repeatable remote export payload ".repeat(4_000),
        };
        let frame = encode_remote_frame(&message, RemoteFrameLimits::default()).unwrap();
        assert_eq!(frame[9], ENCODING_GZIP);
        let decoded: LargeMessage =
            decode_remote_frame(&frame, RemoteFrameLimits::default()).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn frame_truncation_and_frame_tail_are_rejected() {
        let frame = encode_remote_frame(&probe_request(), RemoteFrameLimits::default()).unwrap();

        let mut truncated = frame.clone();
        truncated.pop();
        assert_eq!(
            decode_remote_frame::<RemoteExportRequest>(&truncated, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::TruncatedFrame
        );

        let mut tailed = frame;
        tailed.push(0);
        assert_eq!(
            decode_remote_frame::<RemoteExportRequest>(&tailed, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::TrailingFrameData
        );
    }

    #[test]
    fn header_limits_are_checked_before_payload_read() {
        let mut frame = vec![0_u8; FRAME_HEADER_BYTES];
        frame[..8].copy_from_slice(&FRAME_MAGIC);
        frame[8] = FRAME_VERSION;
        frame[9] = ENCODING_GZIP;
        frame[12..16].copy_from_slice(&(MAX_REMOTE_FRAME_ENCODED_BYTES as u32 + 1).to_be_bytes());
        assert_eq!(
            decode_remote_frame::<RemoteExportRequest>(&frame, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::EncodedLimitExceeded
        );

        frame[12..16].copy_from_slice(&0_u32.to_be_bytes());
        frame[16..20].copy_from_slice(&(MAX_REMOTE_FRAME_DECODED_BYTES as u32 + 1).to_be_bytes());
        assert_eq!(
            decode_remote_frame::<RemoteExportRequest>(&frame, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::DecodedLimitExceeded
        );
    }

    #[test]
    fn identity_length_mismatch_is_rejected() {
        let mut frame =
            encode_remote_frame(&probe_request(), RemoteFrameLimits::default()).unwrap();
        let decoded = read_u32(&frame[16..20]);
        frame[16..20].copy_from_slice(&(decoded + 1).to_be_bytes());
        assert_eq!(
            decode_remote_frame::<RemoteExportRequest>(&frame, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::LengthMismatch
        );
    }

    #[test]
    fn gzip_declared_length_bomb_is_rejected() {
        let message = LargeMessage {
            text: "x".repeat(128 * 1024),
        };
        let mut frame = encode_remote_frame(&message, RemoteFrameLimits::default()).unwrap();
        frame[16..20].copy_from_slice(&(MAX_REMOTE_FRAME_DECODED_BYTES as u32 + 1).to_be_bytes());
        assert_eq!(
            decode_remote_frame::<LargeMessage>(&frame, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::DecodedLimitExceeded
        );
    }

    #[test]
    fn gzip_crc_corruption_is_rejected() {
        let message = LargeMessage {
            text: "payload with crc ".repeat(4_000),
        };
        let mut frame = encode_remote_frame(&message, RemoteFrameLimits::default()).unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 0x80;
        assert_eq!(
            decode_remote_frame::<LargeMessage>(&frame, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::Compression
        );
    }

    #[test]
    fn gzip_payload_tail_and_concatenated_member_are_rejected() {
        let message = LargeMessage {
            text: "payload with tail ".repeat(4_000),
        };
        let frame = encode_remote_frame(&message, RemoteFrameLimits::default()).unwrap();
        assert_eq!(frame[9], ENCODING_GZIP);

        for tail in [vec![1, 2, 3], frame[FRAME_HEADER_BYTES..].to_vec()] {
            let mut tailed = frame.clone();
            tailed.extend_from_slice(&tail);
            let encoded_len = tailed.len() - FRAME_HEADER_BYTES;
            tailed[12..16].copy_from_slice(&(encoded_len as u32).to_be_bytes());
            assert_eq!(
                decode_remote_frame::<LargeMessage>(&tailed, RemoteFrameLimits::default())
                    .unwrap_err()
                    .kind(),
                RemoteProtocolErrorKind::TrailingCompressedData
            );
        }
    }

    #[test]
    fn invalid_magic_version_encoding_and_flags_are_rejected() {
        let frame = encode_remote_frame(&probe_request(), RemoteFrameLimits::default()).unwrap();
        for (index, value, expected) in [
            (0, b'X', RemoteProtocolErrorKind::InvalidMagic),
            (8, 2, RemoteProtocolErrorKind::UnsupportedFrameVersion),
            (9, 9, RemoteProtocolErrorKind::UnsupportedEncoding),
            (10, 1, RemoteProtocolErrorKind::InvalidHeader),
        ] {
            let mut invalid = frame.clone();
            invalid[index] = value;
            assert_eq!(
                decode_remote_frame::<RemoteExportRequest>(&invalid, RemoteFrameLimits::default())
                    .unwrap_err()
                    .kind(),
                expected
            );
        }
    }

    #[test]
    fn fact_pages_enforce_staging_and_activation_boundaries() {
        let intermediate = FactSnapshotPage {
            thread_id: "01abcdef".parse().unwrap(),
            snapshot_id: "snapshot-1".parse().unwrap(),
            fact_generation: nonzero64(3),
            snapshot_watermark: 22,
            next_page_token: Some("page-2".parse().unwrap()),
            activate_fact_cursor: None,
            has_more: true,
        };
        let valid = response_with(RemoteExportResponseBody::FactSnapshot {
            page: intermediate,
            payload: EmptyRemotePayload {},
        });
        encode_remote_frame(&valid, RemoteFrameLimits::default()).unwrap();

        let invalid = response_with(RemoteExportResponseBody::FactDelta {
            page: FactDeltaPage {
                thread_id: "01abcdef".parse().unwrap(),
                batch_id: "batch-1".parse().unwrap(),
                fact_generation: nonzero64(3),
                delta_watermark: 22,
                next_page_token: None,
                activate_fact_cursor: None,
                has_more: false,
            },
            payload: EmptyRemotePayload {},
        });
        assert_eq!(
            encode_remote_frame(&invalid, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );
    }

    #[test]
    fn probe_capability_duplicates_fail_closed_after_decode() {
        let response = response_with(RemoteExportResponseBody::Probe(ProbeResult {
            capabilities: vec![RemoteCapability::GzipFrame, RemoteCapability::GzipFrame],
            state_writable: true,
            rollout_readable: true,
        }));
        assert_eq!(
            encode_remote_frame(&response, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );
    }

    #[test]
    fn future_protocol_and_zero_generation_fail_closed() {
        let mut request = probe_request();
        request.protocol_version += 1;
        assert_eq!(
            encode_remote_frame(&request, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidMessage
        );

        let mut value = serde_json::to_value(probe_request()).unwrap();
        value["expectedSource"]["generation"] = json!(0);
        let frame = raw_identity_frame(&serde_json::to_vec(&value).unwrap());
        assert_eq!(
            decode_remote_frame::<RemoteExportRequest>(&frame, RemoteFrameLimits::default())
                .unwrap_err()
                .kind(),
            RemoteProtocolErrorKind::InvalidJson
        );
    }
}
