use ratatui::layout::Rect;

use super::ScrollbarHitbox;

pub(super) fn reveal_offset(
    offset: usize,
    selected: usize,
    item_count: usize,
    capacity: usize,
) -> usize {
    let max_offset = item_count.saturating_sub(capacity);
    let offset = offset.min(max_offset);
    if capacity == 0 || selected < offset {
        selected.min(max_offset)
    } else if selected >= offset.saturating_add(capacity) {
        selected
            .saturating_add(1)
            .saturating_sub(capacity)
            .min(max_offset)
    } else {
        offset
    }
}

pub(super) fn scroll_offset(
    offset: usize,
    item_count: usize,
    capacity: usize,
    down: bool,
    lines: usize,
) -> usize {
    let max_offset = item_count.saturating_sub(capacity);
    if down {
        offset.saturating_add(lines).min(max_offset)
    } else {
        offset.saturating_sub(lines)
    }
}

pub(super) fn scrollbar_geometry(
    track: Rect,
    item_count: usize,
    capacity: usize,
    offset: usize,
) -> Option<ScrollbarHitbox> {
    if track.width == 0 || track.height < 2 || capacity == 0 || item_count <= capacity {
        return None;
    }
    let track_height = usize::from(track.height);
    let thumb_height = track_height
        .saturating_mul(capacity)
        .div_ceil(item_count)
        .clamp(1, track_height - 1);
    let max_offset = item_count - capacity;
    let travel = track_height - thumb_height;
    let thumb_offset = scale_rounded(offset.min(max_offset), travel, max_offset);
    Some(ScrollbarHitbox {
        track,
        thumb: Rect::new(
            track.x,
            track
                .y
                .saturating_add(u16::try_from(thumb_offset).unwrap_or(u16::MAX)),
            1,
            u16::try_from(thumb_height).unwrap_or(track.height),
        ),
        max_offset,
    })
}

pub(super) fn scale_rounded(value: usize, scale: usize, denominator: usize) -> usize {
    if denominator == 0 {
        return 0;
    }
    let denominator = denominator as u128;
    let scaled = ((value as u128) * (scale as u128) + denominator / 2) / denominator;
    usize::try_from(scaled).unwrap_or(usize::MAX)
}
