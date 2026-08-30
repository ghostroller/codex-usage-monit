use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) fn short_thread_id(thread_id: &str) -> &str {
    thread_id.get(..8).unwrap_or(thread_id)
}

pub(super) fn grapheme_count(value: &str) -> usize {
    UnicodeSegmentation::graphemes(value, true).count()
}

pub(super) fn byte_index_at_grapheme(value: &str, grapheme_index: usize) -> usize {
    value
        .grapheme_indices(true)
        .nth(grapheme_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(value.len())
}

fn grapheme_cursor_after_byte(value: &str, byte_index: usize) -> usize {
    if byte_index == 0 {
        return 0;
    }
    value
        .grapheme_indices(true)
        .position(|(start, grapheme)| start + grapheme.len() >= byte_index)
        .map_or_else(|| grapheme_count(value), |index| index + 1)
}

pub(super) fn insert_at_grapheme_cursor(value: &mut String, cursor: &mut usize, character: char) {
    let byte_index = byte_index_at_grapheme(value, *cursor);
    value.insert(byte_index, character);
    *cursor = grapheme_cursor_after_byte(value, byte_index + character.len_utf8());
}

pub(super) fn backspace_grapheme(value: &mut String, cursor: &mut usize) -> bool {
    *cursor = (*cursor).min(grapheme_count(value));
    if *cursor == 0 {
        return false;
    }
    let start = byte_index_at_grapheme(value, *cursor - 1);
    let end = byte_index_at_grapheme(value, *cursor);
    value.replace_range(start..end, "");
    *cursor -= 1;
    true
}

pub(super) fn delete_grapheme(value: &mut String, cursor: &mut usize) -> bool {
    let count = grapheme_count(value);
    *cursor = (*cursor).min(count);
    if *cursor == count {
        return false;
    }
    let start = byte_index_at_grapheme(value, *cursor);
    let end = byte_index_at_grapheme(value, *cursor + 1);
    value.replace_range(start..end, "");
    true
}

pub(super) fn search_cursor_window(
    value: &str,
    cursor: usize,
    max_width: usize,
) -> (String, String, bool) {
    if max_width == 0 {
        return (String::new(), String::new(), false);
    }
    let graphemes = UnicodeSegmentation::graphemes(value, true).collect::<Vec<_>>();
    let cursor = cursor.min(graphemes.len());
    let content_width = max_width - 1;
    let left_target = content_width / 2;
    let mut left = cursor;
    let mut right = cursor;

    while left > 0 {
        let candidate = graphemes[left - 1..cursor].concat();
        if UnicodeWidthStr::width(candidate.as_str()) > left_target {
            break;
        }
        left -= 1;
    }
    while right < graphemes.len() {
        let candidate = graphemes[left..right + 1].concat();
        if UnicodeWidthStr::width(candidate.as_str()) > content_width {
            break;
        }
        right += 1;
    }
    while left > 0 {
        let candidate = graphemes[left - 1..right].concat();
        if UnicodeWidthStr::width(candidate.as_str()) > content_width {
            break;
        }
        left -= 1;
    }

    (
        graphemes[left..cursor].concat(),
        graphemes[cursor..right].concat(),
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
    let graphemes = UnicodeSegmentation::graphemes(value, true).collect::<Vec<_>>();
    let mut start = graphemes.len();
    while start > 0 {
        let suffix = graphemes[start - 1..].concat();
        let candidate = format!("<{suffix}");
        if UnicodeWidthStr::width(candidate.as_str()) > max_width {
            break;
        }
        start -= 1;
    }
    format!("<{}", graphemes[start..].concat())
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
    for grapheme in UnicodeSegmentation::graphemes(value, true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > content_width {
            break;
        }
        output.push_str(grapheme);
        width += grapheme_width;
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
    for grapheme in UnicodeSegmentation::graphemes(value, true) {
        let width = UnicodeWidthStr::width(grapheme);
        if prefix_width + width > prefix_target {
            break;
        }
        prefix.push_str(grapheme);
        prefix_width += width;
    }

    let mut suffix = Vec::new();
    let mut suffix_width = 0;
    for grapheme in UnicodeSegmentation::graphemes(value, true).rev() {
        let width = UnicodeWidthStr::width(grapheme);
        if suffix_width + width > suffix_target {
            break;
        }
        suffix.push(grapheme);
        suffix_width += width;
    }
    suffix.reverse();
    format!("{prefix}…{}", suffix.concat())
}
