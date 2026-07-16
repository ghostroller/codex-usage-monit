use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn short_thread_id(thread_id: &str) -> &str {
    thread_id.get(..8).unwrap_or(thread_id)
}

pub(super) fn byte_index_at_char(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(value.len())
}

pub(super) fn search_cursor_window(
    value: &str,
    cursor: usize,
    max_width: usize,
) -> (String, String, bool) {
    if max_width == 0 {
        return (String::new(), String::new(), false);
    }
    let chars = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let content_width = max_width - 1;
    let left_target = content_width / 2;
    let mut left = cursor;
    let mut right = cursor;

    while left > 0 {
        let candidate = chars[left - 1..cursor].iter().collect::<String>();
        if UnicodeWidthStr::width(candidate.as_str()) > left_target {
            break;
        }
        left -= 1;
    }
    while right < chars.len() {
        let candidate = chars[left..right + 1].iter().collect::<String>();
        if UnicodeWidthStr::width(candidate.as_str()) > content_width {
            break;
        }
        right += 1;
    }
    while left > 0 {
        let candidate = chars[left - 1..right].iter().collect::<String>();
        if UnicodeWidthStr::width(candidate.as_str()) > content_width {
            break;
        }
        left -= 1;
    }

    (
        chars[left..cursor].iter().collect(),
        chars[cursor..right].iter().collect(),
        true,
    )
}

pub(super) fn compact_search_text(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let chars = value.chars().collect::<Vec<_>>();
    let mut start = chars.len();
    while start > 0 {
        let suffix = chars[start - 1..].iter().collect::<String>();
        let candidate = format!("<{suffix}");
        if UnicodeWidthStr::width(candidate.as_str()) > max_width {
            break;
        }
        start -= 1;
    }
    format!("<{}", chars[start..].iter().collect::<String>())
}

pub(super) fn truncate_display_text(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let content_width = max_width - 1;
    let mut width = 0;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push('…');
    output
}

pub(super) fn truncate_middle_display_text(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let content_width = max_width - 1;
    let prefix_target = content_width / 3;
    let suffix_target = content_width - prefix_target;
    let mut prefix = String::new();
    let mut prefix_width = 0;
    for character in value.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if prefix_width + width > prefix_target {
            break;
        }
        prefix.push(character);
        prefix_width += width;
    }

    let mut suffix = Vec::new();
    let mut suffix_width = 0;
    for character in value.chars().rev() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if suffix_width + width > suffix_target {
            break;
        }
        suffix.push(character);
        suffix_width += width;
    }
    suffix.reverse();
    format!("{prefix}…{}", suffix.into_iter().collect::<String>())
}
