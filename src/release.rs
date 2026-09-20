//! One release identity and one artifact per platform, shared by installers and
//! remote deployment. Archive bytes and executable bytes have separate hashes.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

pub(crate) const MAX_BINARY: u64 = 128 * 1024 * 1024;
const MAX_MANIFEST: u64 = 64 * 1024;
pub(crate) const TARGETS: &[&str] = &[
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
];

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReleaseManifest {
    pub schema_version: u32,
    pub product: String,
    pub version: String,
    pub build_id: String,
    pub protocol_version: u32,
    pub artifacts: Vec<ReleaseArtifact>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReleaseArtifact {
    pub target: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
    pub binary_size: u64,
    pub binary_sha256: String,
}

pub(crate) fn artifact_name(target: &str) -> String {
    format!(
        "codex-usage-monit-{target}{}",
        if target.contains("windows") {
            ".exe"
        } else {
            ".tar.gz"
        }
    )
}

pub(crate) fn is_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "release_invalid: expected a regular file"
    );
    let file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() <= max,
        "release_invalid: file exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= max,
        "release_invalid: file exceeds size limit"
    );
    Ok(bytes)
}

impl ReleaseManifest {
    pub fn read(path: &Path) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(&read_bounded(path, MAX_MANIFEST)?)
            .context("release_manifest_invalid")?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && self.product == "codex-usage-monit"
                && is_hash(&self.build_id)
                && self.protocol_version > 0
                && !self.version.is_empty()
                && self.version.len() <= 80
                && self
                    .version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')),
            "release_manifest_invalid: unsupported identity"
        );
        ensure!(
            !self.artifacts.is_empty() && self.artifacts.len() <= TARGETS.len(),
            "release_manifest_invalid: invalid artifact count"
        );
        let mut seen = std::collections::HashSet::new();
        for a in &self.artifacts {
            ensure!(
                TARGETS.contains(&a.target.as_str())
                    && seen.insert(a.target.as_str())
                    && a.file == artifact_name(&a.target)
                    && a.size > 0
                    && a.size <= MAX_BINARY
                    && a.binary_size > 0
                    && a.binary_size <= MAX_BINARY
                    && is_hash(&a.sha256)
                    && is_hash(&a.binary_sha256),
                "release_manifest_invalid: invalid or duplicate artifact"
            );
            if a.target.contains("windows") {
                ensure!(
                    a.size == a.binary_size && a.sha256 == a.binary_sha256,
                    "release_manifest_invalid: executable hashes disagree"
                );
            }
        }
        Ok(())
    }

    pub fn artifact(&self, target: &str) -> Result<&ReleaseArtifact> {
        self.artifacts
            .iter()
            .find(|a| a.target == target)
            .context("release_artifact_missing: manifest does not contain this platform")
    }

    pub fn check_identity(&self, expected: &crate::remote_agent_manager::AgentInfo) -> Result<()> {
        ensure!(
            self.version == expected.version
                && self.build_id == expected.build_id
                && self.protocol_version == expected.protocol_version,
            "agent_artifact_mismatch: release does not match the center's exact version/build/protocol"
        );
        self.artifact(&expected.target)?;
        Ok(())
    }

    pub fn verify_current(&self, target: &str, version: Option<&str>) -> Result<()> {
        let actual = crate::remote_agent_manager::AgentInfo::local().with_checksum()?;
        self.check_identity(&actual)?;
        ensure!(
            actual.target == target,
            "release_target_mismatch: candidate platform differs"
        );
        if let Some(version) = version.filter(|v| *v != "latest") {
            ensure!(
                self.version == version.trim_start_matches('v'),
                "release_version_mismatch: requested version differs"
            );
        }
        let artifact = self.artifact(target)?;
        ensure!(
            actual.executable_sha256.as_deref() == Some(&artifact.binary_sha256)
                && fs::metadata(std::env::current_exe()?)?.len() == artifact.binary_size,
            "release_checksum_mismatch: candidate differs from release manifest"
        );
        Ok(())
    }
}

pub(crate) fn read_bundle_binary(
    directory: &Path,
    expected: &crate::remote_agent_manager::AgentInfo,
) -> Result<Vec<u8>> {
    let manifest = ReleaseManifest::read(&directory.join("release-manifest.json"))?;
    manifest.check_identity(expected)?;
    let artifact = manifest.artifact(&expected.target)?;
    let bytes = read_bounded(&directory.join(&artifact.file), MAX_BINARY)?;
    ensure!(
        bytes.len() as u64 == artifact.size
            && format!("{:x}", Sha256::digest(&bytes)) == artifact.sha256,
        "agent_checksum_mismatch: release asset differs from manifest"
    );
    let binary = if expected.target.contains("windows") {
        bytes
    } else {
        unpack_binary(&bytes, artifact.binary_size)?
    };
    ensure!(
        binary.len() as u64 == artifact.binary_size
            && format!("{:x}", Sha256::digest(&binary)) == artifact.binary_sha256,
        "agent_checksum_mismatch: executable differs from manifest"
    );
    Ok(binary)
}

/// The distribution archive has exactly one ordinary short-name member. A
/// narrow decoder avoids writing archive-controlled paths or following links.
fn unpack_binary(bytes: &[u8], expected_size: u64) -> Result<Vec<u8>> {
    let decoder = flate2::read::MultiGzDecoder::new(bytes);
    let mut tar = Vec::new();
    let limit = expected_size
        .checked_add(64 * 1024)
        .context("release_invalid: size overflow")?;
    decoder.take(limit + 1).read_to_end(&mut tar)?;
    ensure!(
        tar.len() as u64 <= limit && tar.len() >= 1536 && tar.len().is_multiple_of(512),
        "release_archive_invalid: malformed/oversize tar"
    );
    let header = &tar[..512];
    let nul = header[..100].iter().position(|b| *b == 0).unwrap_or(100);
    ensure!(
        &header[..nul] == b"codex-usage-monit"
            && header[nul..100].iter().all(|b| *b == 0)
            && matches!(header[156], 0 | b'0')
            && header[157..257].iter().all(|b| *b == 0)
            && header[345..500].iter().all(|b| *b == 0),
        "release_archive_invalid: expected one regular binary member"
    );
    let octal = |field: &[u8]| -> Result<u64> {
        let s = std::str::from_utf8(field)?.trim_matches(['\0', ' ']);
        ensure!(
            !s.is_empty() && s.bytes().all(|b| (b'0'..=b'7').contains(&b)),
            "release_archive_invalid: invalid numeric field"
        );
        Ok(u64::from_str_radix(s, 8)?)
    };
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(*b)
            }
        })
        .sum();
    ensure!(
        octal(&header[148..156])? == sum && octal(&header[124..136])? == expected_size,
        "release_archive_invalid: header checksum or size differs"
    );
    let end = 512_usize
        .checked_add(usize::try_from(expected_size)?)
        .context("release_archive_invalid: size overflow")?;
    let padded_end = end.div_ceil(512) * 512;
    if tar.len() < padded_end + 1024 || tar[end..].iter().any(|b| *b != 0) {
        bail!("release_archive_invalid: extra member, truncated archive or invalid padding");
    }
    Ok(tar[512..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive(kind: u8, name: &[u8], tail: bool) -> Vec<u8> {
        let mut tar = vec![0; 2048];
        tar[..name.len()].copy_from_slice(name);
        tar[124..136].copy_from_slice(b"00000000003\0");
        tar[156] = kind;
        tar[148..156].fill(b' ');
        let sum: u64 = tar[..512].iter().map(|b| u64::from(*b)).sum();
        tar[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        tar[512..515].copy_from_slice(b"bin");
        if tail {
            tar[1024] = b'x';
        }
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&tar).unwrap();
        gzip.finish().unwrap()
    }

    #[test]
    fn release_archive_accepts_only_one_bounded_regular_binary() {
        assert_eq!(
            unpack_binary(&archive(b'0', b"codex-usage-monit", false), 3).unwrap(),
            b"bin"
        );
        for bytes in [
            archive(b'2', b"codex-usage-monit", false),
            archive(b'0', b"../codex-usage-monit", false),
            archive(b'0', b"codex-usage-monit", true),
        ] {
            assert!(unpack_binary(&bytes, 3).is_err());
        }
        assert!(unpack_binary(&archive(b'0', b"codex-usage-monit", false), 2).is_err());
    }

    #[test]
    fn release_manifest_rejects_duplicate_targets_and_untrusted_names() {
        let mut m = ReleaseManifest {
            schema_version: 1,
            product: "codex-usage-monit".into(),
            version: "0.6.0".into(),
            build_id: "1".repeat(64),
            protocol_version: 5,
            artifacts: vec![ReleaseArtifact {
                target: TARGETS[0].into(),
                file: artifact_name(TARGETS[0]),
                size: 10,
                sha256: "a".repeat(64),
                binary_size: 20,
                binary_sha256: "b".repeat(64),
            }],
        };
        m.validate().unwrap();
        m.artifacts[0].file = "../unexpected".into();
        assert!(m.validate().is_err());
        m.artifacts[0].file = artifact_name(TARGETS[0]);
        let duplicate =
            serde_json::from_value(serde_json::to_value(&m.artifacts[0]).unwrap()).unwrap();
        m.artifacts.push(duplicate);
        assert!(m.validate().is_err());
    }
}
