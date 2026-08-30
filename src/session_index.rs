use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::bounded_io::{BoundedLine, read_bounded_line};

const SESSION_INDEX_MAX_LINE_BYTES: usize = 1024 * 1024;
const SESSION_INDEX_MAX_ENTRIES: usize = 50_000;
const SESSION_INDEX_MAX_ID_BYTES: usize = 512;
const SESSION_INDEX_MAX_TITLE_BYTES: usize = 4 * 1024;

#[derive(Debug, Deserialize)]
struct SessionIndexEntry {
    id: String,
    #[serde(alias = "threadName")]
    thread_name: String,
    #[serde(default, alias = "updatedAt")]
    updated_at: Option<String>,
}

#[derive(Debug)]
struct IndexedTitle {
    title: String,
    updated_at: Option<DateTime<Utc>>,
    line_number: usize,
}

#[derive(Debug, Default)]
pub(crate) struct LoadedThreadTitles {
    pub(crate) titles: HashMap<String, String>,
    pub(crate) next_update_at: Option<DateTime<Utc>>,
}

pub(crate) fn load_thread_titles(
    codex_home: &Path,
    as_of: DateTime<Utc>,
) -> Result<LoadedThreadTitles> {
    let path = codex_home.join("session_index.jsonl");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(LoadedThreadTitles::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not open {}", path.display()));
        }
    };

    load_thread_titles_reader(BufReader::new(file), &path, as_of)
}

fn load_thread_titles_reader(
    mut reader: impl BufRead,
    path: &Path,
    as_of: DateTime<Utc>,
) -> Result<LoadedThreadTitles> {
    load_thread_titles_reader_with_limits(
        &mut reader,
        path,
        as_of,
        SESSION_INDEX_MAX_LINE_BYTES,
        SESSION_INDEX_MAX_ENTRIES,
        SESSION_INDEX_MAX_TITLE_BYTES,
    )
}

fn load_thread_titles_reader_with_limits(
    reader: &mut impl BufRead,
    path: &Path,
    as_of: DateTime<Utc>,
    max_line_bytes: usize,
    max_entries: usize,
    max_title_bytes: usize,
) -> Result<LoadedThreadTitles> {
    let mut indexed = HashMap::<String, IndexedTitle>::new();
    let mut retained_by_line = BTreeSet::<(usize, String)>::new();
    let mut next_update_at = None;
    let mut line = Vec::new();
    let mut line_number = 0_usize;
    loop {
        let status = read_bounded_line(reader, &mut line, max_line_bytes).with_context(|| {
            format!(
                "could not read {} line {}",
                path.display(),
                line_number.saturating_add(1)
            )
        })?;
        if status == BoundedLine::Eof {
            break;
        }
        line_number = line_number.saturating_add(1);
        if status == BoundedLine::TooLong {
            continue;
        }

        let Ok(entry) = serde_json::from_slice::<SessionIndexEntry>(&line) else {
            continue;
        };
        let title = entry.thread_name.trim();
        if entry.id.is_empty()
            || entry.id.len() > SESSION_INDEX_MAX_ID_BYTES
            || title.is_empty()
            || max_entries == 0
        {
            continue;
        }
        let title = truncate_utf8(title, max_title_bytes);
        if title.is_empty() {
            continue;
        }
        let updated_at = entry
            .updated_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        if let Some(updated_at) = updated_at
            && updated_at > as_of
        {
            if next_update_at.is_none_or(|current| updated_at < current) {
                next_update_at = Some(updated_at);
            }
            continue;
        }
        let replace =
            indexed
                .get(&entry.id)
                .is_none_or(|current| match (current.updated_at, updated_at) {
                    (Some(current_timestamp), Some(candidate)) => {
                        candidate > current_timestamp
                            || (candidate == current_timestamp && line_number > current.line_number)
                    }
                    _ => line_number > current.line_number,
                });
        if replace {
            if let Some(current) = indexed.get(&entry.id) {
                retained_by_line.remove(&(current.line_number, entry.id.clone()));
            } else if indexed.len() >= max_entries
                && let Some((oldest_line, oldest_id)) = retained_by_line.first().cloned()
            {
                retained_by_line.remove(&(oldest_line, oldest_id.clone()));
                indexed.remove(&oldest_id);
            }
            retained_by_line.insert((line_number, entry.id.clone()));
            indexed.insert(
                entry.id,
                IndexedTitle {
                    title: title.to_string(),
                    updated_at,
                    line_number,
                },
            );
        }
    }

    Ok(LoadedThreadTitles {
        titles: indexed
            .into_iter()
            .map(|(thread_id, indexed)| (thread_id, indexed.title))
            .collect(),
        next_update_at,
    })
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};
    use std::path::Path;

    use chrono::{TimeZone, Utc};

    use super::load_thread_titles_reader_with_limits;

    #[test]
    fn oversized_lines_are_drained_before_parsing_the_next_entry() {
        let input = format!(
            "{{\"id\":\"oversized\",\"thread_name\":\"{}\"}}\n{{\"id\":\"kept\",\"thread_name\":\"Recovered\"}}\n",
            "x".repeat(128)
        );
        let mut reader = BufReader::with_capacity(7, Cursor::new(input));

        let titles = load_thread_titles_reader_with_limits(
            &mut reader,
            Path::new("session_index.jsonl"),
            chrono::DateTime::<Utc>::MAX_UTC,
            64,
            10,
            32,
        )
        .unwrap();

        assert_eq!(titles.titles.len(), 1);
        assert_eq!(
            titles.titles.get("kept").map(String::as_str),
            Some("Recovered")
        );
    }

    #[test]
    fn entry_and_utf8_title_limits_retain_the_newest_ids() {
        let input = concat!(
            "{\"id\":\"one\",\"thread_name\":\"éééé\"}\n",
            "{\"id\":\"two\",\"thread_name\":\"second\"}\n",
            "{\"id\":\"one\",\"thread_name\":\"new title\"}\n",
            "{\"id\":\"three\",\"thread_name\":\"newest\"}\n"
        );
        let mut reader = BufReader::new(Cursor::new(input));

        let titles = load_thread_titles_reader_with_limits(
            &mut reader,
            Path::new("session_index.jsonl"),
            chrono::DateTime::<Utc>::MAX_UTC,
            256,
            2,
            5,
        )
        .unwrap();

        assert_eq!(titles.titles.len(), 2);
        assert_eq!(titles.titles.get("one").map(String::as_str), Some("new t"));
        assert_eq!(
            titles.titles.get("three").map(String::as_str),
            Some("newes")
        );
        assert!(!titles.titles.contains_key("two"));
    }

    #[test]
    fn future_titles_wait_for_their_updated_at_boundary() {
        let current = Utc.with_ymd_and_hms(2026, 8, 30, 0, 0, 0).unwrap();
        let future = current + chrono::Duration::hours(1);
        let input = format!(
            "{{\"id\":\"thread\",\"thread_name\":\"Current\",\"updated_at\":\"{}\"}}\n{{\"id\":\"thread\",\"thread_name\":\"Future\",\"updated_at\":\"{}\"}}\n",
            (current - chrono::Duration::hours(1)).to_rfc3339(),
            future.to_rfc3339(),
        );

        let mut before_reader = BufReader::new(Cursor::new(input.as_bytes()));
        let before = load_thread_titles_reader_with_limits(
            &mut before_reader,
            Path::new("session_index.jsonl"),
            current,
            1024,
            10,
            128,
        )
        .unwrap();
        assert_eq!(
            before.titles.get("thread").map(String::as_str),
            Some("Current")
        );
        assert_eq!(before.next_update_at, Some(future));

        let mut after_reader = BufReader::new(Cursor::new(input.as_bytes()));
        let after = load_thread_titles_reader_with_limits(
            &mut after_reader,
            Path::new("session_index.jsonl"),
            future,
            1024,
            10,
            128,
        )
        .unwrap();
        assert_eq!(
            after.titles.get("thread").map(String::as_str),
            Some("Future")
        );
        assert!(after.next_update_at.is_none());
    }
}
