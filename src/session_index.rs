use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

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

pub(crate) fn load_thread_titles(codex_home: &Path) -> Result<HashMap<String, String>> {
    let path = codex_home.join("session_index.jsonl");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not open {}", path.display()));
        }
    };

    let mut indexed = HashMap::<String, IndexedTitle>::new();
    for (line_index, line) in BufReader::new(file).lines().enumerate() {
        let line_number = line_index + 1;
        let line =
            line.with_context(|| format!("could not read {} line {line_number}", path.display()))?;
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(&line) else {
            continue;
        };
        let title = entry.thread_name.trim();
        if entry.id.is_empty() || title.is_empty() {
            continue;
        }
        let updated_at = entry
            .updated_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
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

    Ok(indexed
        .into_iter()
        .map(|(thread_id, indexed)| (thread_id, indexed.title))
        .collect())
}
