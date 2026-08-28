use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

const PARTIAL_COLOR_WEIGHT: u16 = 4;
const PARTIAL_BACKGROUND_WEIGHT: u16 = 1;

/// Coverage state for one time bucket in a stacked-area chart.
///
/// Missing buckets are rendered as gaps instead of zero-valued observations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StackedAreaState {
    Complete,
    Partial,
    Missing,
}

/// One bottom-to-top layer in a stacked-area chart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StackedAreaSeries {
    color: Color,
    values: Vec<u128>,
}

impl StackedAreaSeries {
    pub(super) fn new(color: Color, values: Vec<u128>) -> Self {
        Self { color, values }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StackedAreaPixel {
    series_index: usize,
    partial: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StackedAreaCell {
    upper: Option<StackedAreaPixel>,
    lower: Option<StackedAreaPixel>,
}

/// A half-block stacked-area widget.
///
/// Each terminal cell represents two vertical pixels. The upper half is
/// encoded as the foreground of `▀`, while the lower half is encoded as its
/// background. A full cell uses a colored background so adjacent cells have no
/// glyph spacing.
#[derive(Clone, Copy, Debug)]
pub(super) struct StackedArea<'a> {
    series: &'a [StackedAreaSeries],
    states: &'a [StackedAreaState],
    y_max: f64,
    background: Color,
}

impl<'a> StackedArea<'a> {
    pub(super) fn new(
        series: &'a [StackedAreaSeries],
        states: &'a [StackedAreaState],
        y_max: f64,
        background: Color,
    ) -> Self {
        Self {
            series,
            states,
            y_max,
            background,
        }
    }
}

impl Widget for StackedArea<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        let cells = rasterize_stacked_area(
            self.series,
            self.states,
            area.width,
            area.height,
            self.y_max,
        );
        for y in 0..area.height {
            for x in 0..area.width {
                let index = usize::from(y)
                    .saturating_mul(usize::from(area.width))
                    .saturating_add(usize::from(x));
                let Some(cell) = cells.get(index).copied() else {
                    continue;
                };
                if cell == StackedAreaCell::default() {
                    // The axis widget rendered immediately before this widget
                    // owns the empty plot background. Leaving it untouched also
                    // makes missing data a real gap rather than a zero sample.
                    continue;
                }
                let upper = cell.upper.and_then(|pixel| self.pixel_color(pixel));
                let lower = cell.lower.and_then(|pixel| self.pixel_color(pixel));
                let Some(target) =
                    buffer.cell_mut((area.x.saturating_add(x), area.y.saturating_add(y)))
                else {
                    continue;
                };
                target.reset();
                match (upper, lower) {
                    (Some(upper), Some(lower)) if upper == lower => {
                        target.set_symbol(" ").set_fg(upper).set_bg(upper);
                    }
                    (Some(upper), Some(lower)) => {
                        target.set_symbol("▀").set_fg(upper).set_bg(lower);
                    }
                    (Some(upper), None) => {
                        target.set_symbol("▀").set_fg(upper).set_bg(self.background);
                    }
                    (None, Some(lower)) => {
                        target.set_symbol("▄").set_fg(lower).set_bg(self.background);
                    }
                    (None, None) => unreachable!("empty cells are skipped above"),
                }
            }
        }
    }
}

impl StackedArea<'_> {
    fn pixel_color(self, pixel: StackedAreaPixel) -> Option<Color> {
        let color = self.series.get(pixel.series_index)?.color;
        Some(if pixel.partial {
            partial_color(color, self.background)
        } else {
            color
        })
    }
}

fn rasterize_stacked_area(
    series: &[StackedAreaSeries],
    states: &[StackedAreaState],
    width: u16,
    height: u16,
    y_max: f64,
) -> Vec<StackedAreaCell> {
    let cell_count = usize::from(width).saturating_mul(usize::from(height));
    let mut cells = vec![StackedAreaCell::default(); cell_count];
    if width == 0
        || height == 0
        || states.is_empty()
        || series.is_empty()
        || !y_max.is_finite()
        || y_max <= 0.0
        || series
            .iter()
            .any(|candidate| candidate.values.len() != states.len())
    {
        return cells;
    }

    let virtual_height = usize::from(height).saturating_mul(2);
    let mut boundaries = Vec::with_capacity(series.len());
    for x in 0..usize::from(width) {
        let Some(sample) = column_sample(states, x, usize::from(width)) else {
            continue;
        };
        boundaries.clear();
        let mut cumulative = 0.0_f64;
        for candidate in series {
            cumulative += sample.value(&candidate.values);
            let scaled = (cumulative / y_max * virtual_height as f64)
                .round()
                .clamp(0.0, virtual_height as f64) as usize;
            boundaries.push(scaled);
        }

        let filled_height = boundaries.last().copied().unwrap_or_default();
        for bottom_pixel in 0..filled_height {
            let series_index = boundaries.partition_point(|boundary| *boundary <= bottom_pixel);
            if series_index >= series.len() {
                continue;
            }
            let top_pixel = virtual_height.saturating_sub(1 + bottom_pixel);
            let y = top_pixel / 2;
            let index = y.saturating_mul(usize::from(width)).saturating_add(x);
            let Some(cell) = cells.get_mut(index) else {
                continue;
            };
            let pixel = Some(StackedAreaPixel {
                series_index,
                partial: sample.partial,
            });
            if top_pixel % 2 == 0 {
                cell.upper = pixel;
            } else {
                cell.lower = pixel;
            }
        }
    }
    cells
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ColumnValues {
    Interpolated {
        left: usize,
        right: usize,
        fraction: f64,
    },
    Average {
        start: usize,
        end: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ColumnSample {
    values: ColumnValues,
    partial: bool,
}

impl ColumnSample {
    fn value(self, values: &[u128]) -> f64 {
        match self.values {
            ColumnValues::Interpolated {
                left,
                right,
                fraction,
            } => {
                let left = values[left] as f64;
                let right = values[right] as f64;
                left + (right - left) * fraction
            }
            ColumnValues::Average { start, end } => {
                let (sum, count) = values[start..end]
                    .iter()
                    .fold((0_u128, 0_usize), |(sum, count), value| {
                        (sum.saturating_add(*value), count.saturating_add(1))
                    });
                if count == 0 {
                    0.0
                } else {
                    sum as f64 / count as f64
                }
            }
        }
    }
}

fn column_sample(states: &[StackedAreaState], x: usize, width: usize) -> Option<ColumnSample> {
    let day_count = states.len();
    if width < day_count {
        let start = x.saturating_mul(day_count) / width;
        let end = (x.saturating_add(1).saturating_mul(day_count) / width)
            .max(start.saturating_add(1))
            .min(day_count);
        let covered_states = &states[start..end];
        if covered_states.contains(&StackedAreaState::Missing) {
            return None;
        }
        return Some(ColumnSample {
            values: ColumnValues::Average { start, end },
            partial: covered_states.contains(&StackedAreaState::Partial),
        });
    }

    let day_index = date_index_at_column(x, width, day_count)?;
    let state = states[day_index];
    if state == StackedAreaState::Missing {
        return None;
    }
    if day_count == 1 || width == 1 {
        return Some(ColumnSample {
            values: ColumnValues::Interpolated {
                left: day_index,
                right: day_index,
                fraction: 0.0,
            },
            partial: state == StackedAreaState::Partial,
        });
    }

    let position = x as f64 * (day_count - 1) as f64 / (width - 1) as f64;
    let left = (position.floor() as usize).min(day_count - 1);
    let right = (position.ceil() as usize).min(day_count - 1);
    let can_interpolate =
        states[left] != StackedAreaState::Missing && states[right] != StackedAreaState::Missing;
    let (left, right, fraction) = if can_interpolate {
        (left, right, position - left as f64)
    } else {
        (day_index, day_index, 0.0)
    };
    Some(ColumnSample {
        values: ColumnValues::Interpolated {
            left,
            right,
            fraction,
        },
        partial: state == StackedAreaState::Partial,
    })
}

/// Maps a rendered plot column to the nearest exact observation.
///
/// The same mapping is used by the rasterizer and the Summary inspector so a
/// column can never be tinted as one date while reporting another. When there
/// are fewer columns than observations, the renderer aggregates observations;
/// there is no honest exact-date mapping for that column, so inspection is
/// disabled instead of presenting an aggregate as a single day's value.
pub(super) fn date_index_at_column(
    column: usize,
    width: usize,
    observation_count: usize,
) -> Option<usize> {
    if width == 0 || observation_count == 0 || column >= width || width < observation_count {
        return None;
    }
    if width == 1 || observation_count == 1 {
        return Some(0);
    }
    let numerator = column
        .saturating_mul(observation_count - 1)
        .saturating_mul(2)
        .saturating_add(width - 1);
    let denominator = (width - 1).saturating_mul(2);
    Some(
        numerator
            .checked_div(denominator)
            .unwrap_or_default()
            .min(observation_count - 1),
    )
}

fn partial_color(color: Color, background: Color) -> Color {
    let (Color::Rgb(red, green, blue), Color::Rgb(bg_red, bg_green, bg_blue)) = (color, background)
    else {
        return color;
    };
    Color::Rgb(
        blend_channel(red, bg_red),
        blend_channel(green, bg_green),
        blend_channel(blue, bg_blue),
    )
}

fn blend_channel(foreground: u8, background: u8) -> u8 {
    let total_weight = PARTIAL_COLOR_WEIGHT + PARTIAL_BACKGROUND_WEIGHT;
    let value = u16::from(foreground)
        .saturating_mul(PARTIAL_COLOR_WEIGHT)
        .saturating_add(u16::from(background).saturating_mul(PARTIAL_BACKGROUND_WEIGHT))
        .saturating_add(total_weight / 2)
        / total_weight;
    u8::try_from(value).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    const BACKGROUND: Color = Color::Rgb(10, 20, 30);
    const FIRST: Color = Color::Rgb(100, 120, 140);
    const SECOND: Color = Color::Rgb(200, 180, 160);

    fn series(color: Color, values: &[u128]) -> StackedAreaSeries {
        StackedAreaSeries::new(color, values.to_vec())
    }

    #[test]
    fn half_height_value_uses_the_lower_half_pixel() {
        let layers = [series(FIRST, &[1])];
        let states = [StackedAreaState::Complete];
        let cells = rasterize_stacked_area(&layers, &states, 1, 1, 2.0);

        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].upper, None);
        assert_eq!(
            cells[0].lower,
            Some(StackedAreaPixel {
                series_index: 0,
                partial: false,
            })
        );

        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        StackedArea::new(&layers, &states, 2.0, BACKGROUND).render(buffer.area, &mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "▄");
        assert_eq!(buffer[(0, 0)].fg, FIRST);
        assert_eq!(buffer[(0, 0)].bg, BACKGROUND);
    }

    #[test]
    fn two_layers_can_share_one_terminal_cell() {
        let layers = [series(FIRST, &[1]), series(SECOND, &[1])];
        let states = [StackedAreaState::Complete];
        let cells = rasterize_stacked_area(&layers, &states, 1, 1, 2.0);

        assert_eq!(cells[0].upper.unwrap().series_index, 1);
        assert_eq!(cells[0].lower.unwrap().series_index, 0);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        StackedArea::new(&layers, &states, 2.0, BACKGROUND).render(buffer.area, &mut buffer);
        let cell = &buffer[(0, 0)];
        assert_eq!(cell.symbol(), "▀");
        assert_eq!(cell.fg, SECOND);
        assert_eq!(cell.bg, FIRST);
    }

    #[test]
    fn a_full_single_color_cell_uses_a_seamless_background() {
        let layers = [series(FIRST, &[2])];
        let states = [StackedAreaState::Complete];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        StackedArea::new(&layers, &states, 2.0, BACKGROUND).render(buffer.area, &mut buffer);

        let cell = &buffer[(0, 0)];
        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.fg, FIRST);
        assert_eq!(cell.bg, FIRST);
    }

    #[test]
    fn missing_day_owns_a_full_gap_and_is_not_interpolated_across() {
        let cells = rasterize_stacked_area(
            &[series(FIRST, &[4, 100, 2])],
            &[
                StackedAreaState::Complete,
                StackedAreaState::Missing,
                StackedAreaState::Complete,
            ],
            9,
            2,
            4.0,
        );

        for x in 2..6 {
            assert!(
                (0..2).all(|y| cells[y * 9 + x] == StackedAreaCell::default()),
                "column {x} must remain a real gap"
            );
        }
        assert_ne!(cells[10], StackedAreaCell::default());
        assert_ne!(cells[15], StackedAreaCell::default());
    }

    #[test]
    fn exact_date_mapping_is_shared_by_every_wide_plot_column() {
        let expected = (0..52)
            .map(|column| match column {
                0..=8 => 0,
                9..=25 => 1,
                26..=42 => 2,
                _ => 3,
            })
            .collect::<Vec<_>>();
        let actual = (0..52)
            .map(|column| date_index_at_column(column, 52, 4).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
        assert_eq!(date_index_at_column(0, 1, 1), Some(0));
        assert_eq!(date_index_at_column(0, 2, 3), None);
        assert_eq!(date_index_at_column(2, 2, 2), None);
    }

    #[test]
    fn complete_neighbors_are_linearly_interpolated() {
        let cells = rasterize_stacked_area(
            &[series(FIRST, &[0, 4])],
            &[StackedAreaState::Complete, StackedAreaState::Complete],
            3,
            2,
            4.0,
        );

        assert_eq!(cells[1], StackedAreaCell::default());
        assert_eq!(cells[4].upper.unwrap().series_index, 0);
        assert_eq!(cells[4].lower.unwrap().series_index, 0);
    }

    #[test]
    fn narrow_chart_downsamples_every_day_and_preserves_missing_gaps() {
        let states = [
            StackedAreaState::Complete,
            StackedAreaState::Complete,
            StackedAreaState::Missing,
            StackedAreaState::Complete,
            StackedAreaState::Partial,
            StackedAreaState::Complete,
        ];
        let cells = rasterize_stacked_area(
            &[series(FIRST, &[2, 4, 100, 6, 8, 10])],
            &states,
            3,
            2,
            10.0,
        );

        assert!(
            (0..2).all(|y| cells[y * 3 + 1] == StackedAreaCell::default()),
            "the middle output column covers the missing third day"
        );
        assert!(
            (0..2)
                .filter_map(|y| cells[y * 3 + 2].upper.or(cells[y * 3 + 2].lower))
                .all(|pixel| pixel.partial),
            "a compacted column is partial when any represented day is partial"
        );
    }

    #[test]
    fn partial_pixels_keep_the_series_hue_with_a_background_tint() {
        let layers = [series(FIRST, &[2])];
        let states = [StackedAreaState::Partial];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer[(0, 0)].modifier = Modifier::BOLD;
        StackedArea::new(&layers, &states, 2.0, BACKGROUND).render(buffer.area, &mut buffer);

        let expected = Color::Rgb(82, 100, 118);
        let cell = &buffer[(0, 0)];
        assert_eq!(cell.symbol(), " ");
        assert_eq!(cell.fg, expected);
        assert_eq!(cell.bg, expected);
        assert_eq!(cell.modifier, Modifier::empty());
        assert_ne!(cell.fg, Color::Yellow);
    }

    #[test]
    fn blank_and_invalid_inputs_leave_the_existing_buffer_untouched() {
        let area = Rect::new(0, 0, 2, 1);
        let mut buffer = Buffer::empty(area);
        buffer[(0, 0)]
            .set_symbol("x")
            .set_fg(Color::Green)
            .set_bg(Color::Blue);
        let invalid = [series(FIRST, &[1])];
        StackedArea::new(
            &invalid,
            &[StackedAreaState::Complete, StackedAreaState::Complete],
            1.0,
            BACKGROUND,
        )
        .render(area, &mut buffer);

        assert_eq!(buffer[(0, 0)].symbol(), "x");
        assert_eq!(buffer[(0, 0)].fg, Color::Green);
        assert_eq!(buffer[(0, 0)].bg, Color::Blue);
        assert!(rasterize_stacked_area(&invalid, &[], 0, 0, 0.0).is_empty());
    }

    #[test]
    fn values_above_the_axis_maximum_are_clamped_to_the_plot() {
        let cells = rasterize_stacked_area(
            &[series(FIRST, &[u128::MAX])],
            &[StackedAreaState::Complete],
            2,
            2,
            1.0,
        );

        assert_eq!(cells.len(), 4);
        assert!(cells.iter().all(|cell| {
            cell.upper.is_some_and(|pixel| pixel.series_index == 0)
                && cell.lower.is_some_and(|pixel| pixel.series_index == 0)
        }));
    }
}
