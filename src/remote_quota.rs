//! Source-owned quota observations carried by the bounded delta journal.

use std::io;
use std::num::NonZeroU64;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::domain::Provenance;
use crate::history::QuotaPoint;

pub const MAX_QUOTA_POINTS_PER_DAY: usize = 4096;

/// A finite percentage. The private field and checked deserializer make Eq
/// valid while preserving the server's fractional precision on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct QuotaPercentage(f64);
impl Eq for QuotaPercentage {}

impl QuotaPercentage {
    fn new(value: f64) -> io::Result<Self> {
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            return Err(invalid("quota percentage must be finite and within 0..100"));
        }
        Ok(Self(if value == 0.0 { 0.0 } else { value }))
    }
}

impl<'de> Deserialize<'de> for QuotaPercentage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteQuotaPoint {
    pub observed_at: DateTime<Utc>,
    pub limit_id: String,
    pub duration_mins: i64,
    pub resets_at: DateTime<Utc>,
    pub used_percent: QuotaPercentage,
    pub remaining_percent: QuotaPercentage,
    pub provenance: Provenance,
}

impl RemoteQuotaPoint {
    pub(crate) fn from_local(point: &QuotaPoint) -> io::Result<Self> {
        crate::source_history::validate_account_quota_point(point)?;
        Ok(Self {
            observed_at: point.observed_at,
            limit_id: point.limit_id.clone(),
            duration_mins: point.duration_mins,
            resets_at: point.resets_at,
            used_percent: QuotaPercentage::new(point.used_percent)?,
            remaining_percent: QuotaPercentage::new(point.remaining_percent)?,
            provenance: point.provenance,
        })
    }

    pub(crate) fn to_local(&self) -> QuotaPoint {
        QuotaPoint {
            observed_at: self.observed_at,
            limit_id: self.limit_id.clone(),
            duration_mins: self.duration_mins,
            resets_at: self.resets_at,
            used_percent: self.used_percent.0,
            remaining_percent: self.remaining_percent.0,
            provenance: self.provenance,
        }
    }

    pub(crate) fn sort_key(&self) -> (DateTime<Utc>, &str, i64, DateTime<Utc>) {
        (
            self.observed_at,
            &self.limit_id,
            self.duration_mins,
            self.resets_at,
        )
    }
}

/// A complete source-local day, or its retention tombstone. No imported quota
/// is included here, so bidirectional synchronization cannot echo observations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteQuotaDay {
    pub day: NaiveDate,
    pub points: Vec<RemoteQuotaPoint>,
}

impl RemoteQuotaDay {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.points.len() > MAX_QUOTA_POINTS_PER_DAY {
            return Err(invalid("remote quota day exceeds its observation bound"));
        }
        for point in &self.points {
            crate::source_history::validate_account_quota_point(&point.to_local())?;
            if !point.limit_id.eq_ignore_ascii_case("codex")
                || !matches!(point.duration_mins, 300 | 10_080)
                || point.provenance != Provenance::ServerSnapshot
                || (point.used_percent.0 + point.remaining_percent.0 - 100.0).abs() > 0.000_001
                || point.observed_at >= point.resets_at
                || point.resets_at.signed_duration_since(point.observed_at)
                    > chrono::Duration::minutes(point.duration_mins)
                        + chrono::Duration::seconds(120)
            {
                return Err(invalid(
                    "remote quota observation is not a valid recorded Codex account window",
                ));
            }
            if point.observed_at.date_naive() != self.day {
                return Err(invalid("remote quota observation is outside its day"));
            }
        }
        if self
            .points
            .windows(2)
            .any(|p| p[0].sort_key() >= p[1].sort_key())
        {
            return Err(invalid(
                "remote quota observations must be sorted and unique",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteQuotaChange {
    pub sequence: NonZeroU64,
    pub quota: RemoteQuotaDay,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn plan_local_quota_records(
    config: &crate::config::CollectConfig,
    identity: &crate::source_identity::SourceIdentityStore,
    observed_at: DateTime<Utc>,
) -> anyhow::Result<Vec<crate::remote_export_state::RemoteExportDesiredRecord>> {
    use crate::remote_delta_journal::{
        RemoteDeltaJournalRecord, encode_remote_delta_journal_record,
    };
    use crate::remote_export_state::RemoteExportDesiredRecord;
    let root = identity
        .path()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| anyhow::anyhow!("quota export requires a durable state root"))?;
    if !root.join("history-v2").try_exists()? {
        return Ok(Vec::new());
    }
    let runtime = crate::history_runtime::HistoryRuntime::new(
        root.join("history-v1"),
        &config.codex_home,
        config.redact_content,
    )?;
    // Deliberately read only locally recorded account shards. The source query
    // is a merged projection and must never be used by an exporter.
    let account = runtime
        .source_history()
        .load_account_since(observed_at - chrono::Duration::days(35))?;
    let mut days = std::collections::BTreeMap::<NaiveDate, Vec<RemoteQuotaPoint>>::new();
    for point in account
        .quota_points
        .iter()
        .filter(|point| point.observed_at <= observed_at)
    {
        days.entry(point.observed_at.date_naive())
            .or_default()
            .push(RemoteQuotaPoint::from_local(point)?);
    }
    let mut records = Vec::new();
    for (day, mut points) in days {
        points.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        let quota = RemoteQuotaDay { day, points };
        quota.validate()?;
        let expires_at = day
            .succ_opt()
            .and_then(|next| next.and_hms_opt(0, 0, 0))
            .ok_or_else(|| invalid("quota retention day overflow"))?
            .and_utc()
            + chrono::Duration::days(35);
        records.push(RemoteExportDesiredRecord::new(
            format!("quota-day-v1:{day}"),
            expires_at,
            encode_remote_delta_journal_record(
                RemoteDeltaJournalRecord::QuotaDay(quota),
                Vec::new(),
            )?,
            encode_remote_delta_journal_record(
                RemoteDeltaJournalRecord::QuotaDay(RemoteQuotaDay {
                    day,
                    points: Vec::new(),
                }),
                Vec::new(),
            )?,
        )?);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_wire_rejects_invalid_percentages_windows_and_duplicate_observations() {
        for value in [-1.0, 100.01, f64::INFINITY, f64::NAN] {
            assert!(QuotaPercentage::new(value).is_err());
        }
        assert!(serde_json::from_str::<QuotaPercentage>("101").is_err());
        let observed_at = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let point = RemoteQuotaPoint::from_local(&QuotaPoint {
            observed_at,
            limit_id: "codex".into(),
            duration_mins: 300,
            resets_at: observed_at + chrono::Duration::hours(4),
            used_percent: 12.3456,
            remaining_percent: 87.6544,
            provenance: Provenance::ServerSnapshot,
        })
        .unwrap();
        let mut day = RemoteQuotaDay {
            day: observed_at.date_naive(),
            points: vec![point.clone()],
        };
        day.validate().unwrap();
        let bytes = serde_json::to_vec(&day).unwrap();
        assert_eq!(
            serde_json::from_slice::<RemoteQuotaDay>(&bytes).unwrap(),
            day
        );
        day.points.push(point.clone());
        assert!(day.validate().is_err());
        day.points = vec![point];
        day.points[0].remaining_percent = QuotaPercentage::new(10.0).unwrap();
        assert!(day.validate().is_err());
    }
}
