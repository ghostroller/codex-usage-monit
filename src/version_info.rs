//! Version identity and release notes bundled with the running executable.

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const BUILD_ID: &str = env!("MONIT_BUILD_ID");
pub(crate) const TARGET: &str = env!("MONIT_BUILD_TARGET");

const CHANGELOG: &str = include_str!("../CHANGELOG.md");

pub(crate) struct ReleaseNotes<'a> {
    pub date: &'a str,
    pub body: &'a str,
}

pub(crate) fn current_release_notes() -> Option<ReleaseNotes<'static>> {
    release_notes_for(CHANGELOG, VERSION)
}

fn release_notes_for<'a>(changelog: &'a str, version: &str) -> Option<ReleaseNotes<'a>> {
    let heading = format!("## [{version}]");
    let mut body_start = None;
    let mut date = "Unknown";
    let mut offset = 0;
    for line_with_ending in changelog.split_inclusive('\n') {
        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        if line.starts_with("## ") {
            if let Some(start) = body_start {
                return Some(ReleaseNotes {
                    date,
                    body: changelog[start..offset].trim(),
                });
            }
            if let Some(suffix) = line.strip_prefix(&heading) {
                // Accept the changelog's dated and undated heading forms while
                // rejecting version prefixes or other text after the version.
                let suffix = suffix.trim();
                if suffix.is_empty() || suffix.starts_with("- ") {
                    date = suffix
                        .strip_prefix("- ")
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .unwrap_or("Unknown");
                    body_start = Some(offset + line_with_ending.len());
                }
            }
        }
        offset += line_with_ending.len();
    }
    body_start.map(|start| ReleaseNotes {
        date,
        body: changelog[start..].trim(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_notes_select_only_the_running_release() {
        let changelog = "# Changelog\n\n## [Unreleased]\n\n- Future feature.\n\n## [1.2.30] - 2026-09-30\n\n- Different version.\n\n## [1.2.3] - 2026-10-04\n\n### Added\n\n- Current feature.\n\n## [1.2.2] - 2026-09-29\n\n- Older feature.\n";
        let notes = release_notes_for(changelog, "1.2.3").unwrap();
        assert_eq!(notes.date, "2026-10-04");
        assert_eq!(notes.body, "### Added\n\n- Current feature.");
    }

    #[test]
    fn version_notes_missing_release_has_no_fallback() {
        let changelog =
            "## [Unreleased]\n- Future feature.\n## [1.2.3] - 2026-10-04\n- Current feature.\n";
        assert!(release_notes_for(changelog, "1.2.4").is_none());
        assert!(release_notes_for(changelog, "1.2").is_none());
    }

    #[test]
    fn version_notes_support_crlf_undated_and_final_sections() {
        let notes =
            release_notes_for("## [1.2.3]\r\n\r\n### Fixed\r\n\r\n- Final fix.", "1.2.3").unwrap();
        assert_eq!(notes.date, "Unknown");
        assert_eq!(notes.body, "### Fixed\r\n\r\n- Final fix.");
    }

    #[test]
    fn version_notes_stop_at_any_next_second_level_heading() {
        let notes = release_notes_for(
            "## [1.2.3] - 2026-10-04\n- Feature.\n## Other notes\n- Unrelated.",
            "1.2.3",
        )
        .unwrap();
        assert_eq!(notes.body, "- Feature.");
    }

    #[test]
    fn version_notes_current_bundle_matches_package_version() {
        let notes =
            current_release_notes().expect("current package version has bundled release notes");
        assert_ne!(notes.date, "Unknown");
        assert!(!notes.body.is_empty());
    }
}
