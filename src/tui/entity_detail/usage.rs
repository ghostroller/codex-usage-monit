use super::*;

#[derive(Clone, Debug)]
pub(in crate::tui) enum UsageBlock {
    Tokens {
        label: String,
        usage: TokenUsage,
    },
    Cost {
        prefix: String,
        amount: ApiCostAmount,
        state: ApiCostWindowState,
    },
    TokenComparison {
        columns: Vec<(String, TokenUsage)>,
        show_composition: bool,
    },
    QuotaComparison {
        columns: Vec<(String, WindowUsage)>,
        account_used_percent: f64,
        long_context: bool,
    },
    CostComparison {
        columns: Vec<(String, ApiCostAmount)>,
        state: ApiCostWindowState,
    },
}

impl UsageBlock {
    pub(super) fn render(&self, width: u16, theme: Theme) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = Vec::new();
        match self {
            Self::Tokens { label, usage } => token_lines(&mut lines, label, *usage, width, theme),
            Self::Cost {
                prefix,
                amount,
                state,
            } => cost_lines(&mut lines, prefix, *amount, *state, width, theme),
            Self::TokenComparison {
                columns,
                show_composition,
            } => token_comparison(&mut lines, columns, *show_composition, width, theme),
            Self::QuotaComparison {
                columns,
                account_used_percent,
                long_context,
            } => quota_comparison(
                &mut lines,
                columns,
                *account_used_percent,
                *long_context,
                width,
                theme,
            ),
            Self::CostComparison { columns, state } => {
                cost_comparison(&mut lines, columns, *state, width, theme)
            }
        }
        // A one-column terminal cannot display a two-column grapheme. Keep the
        // text in the document, while bounding every physical render row.
        let lines = wrapped_lines(&lines, width);
        if width == 1 {
            lines
                .into_iter()
                .map(|line| {
                    if line.width() <= 1 {
                        line
                    } else {
                        Line::styled("…", line.style)
                    }
                })
                .collect()
        } else {
            lines
        }
    }
}

fn number(value: u64) -> String {
    let digits = value.to_string();
    let mut result = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            result.push(',');
        }
        result.push(digit);
    }
    result
}

fn percentage(numerator: u64, denominator: u64) -> String {
    if denominator == 0 {
        "unavailable (zero denominator)".into()
    } else {
        let value = numerator as f64 / denominator as f64 * 100.0;
        if numerator > 0 && value < 0.01 {
            "<0.01%".into()
        } else {
            format!("{value:.2}%")
        }
    }
}

fn value(text: impl Into<String>, color: Color) -> Vec<Span<'static>> {
    vec![Span::styled(
        text.into(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )]
}

fn heading(lines: &mut Vec<Line<'static>>, title: &str, theme: Theme) {
    lines.push(Line::styled(
        terminal_safe_text(title),
        Style::default()
            .fg(theme.palette().title)
            .add_modifier(Modifier::BOLD),
    ));
}

fn note(lines: &mut Vec<Line<'static>>, text: impl Into<String>, theme: Theme) {
    lines.push(Line::styled(
        text.into(),
        Style::default().fg(theme.palette().muted),
    ));
}

fn field_line(label: &str, values: Vec<Span<'static>>, color: Color) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{}: ", terminal_safe_text(label)),
        Style::default().fg(color),
    )];
    spans.extend(values);
    Line::from(spans)
}

struct MetricRow {
    label: &'static str,
    values: Vec<Vec<Span<'static>>>,
    color: Color,
}

fn cell_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn grid_width(headers: &[String], rows: &[MetricRow]) -> (usize, Vec<usize>, usize) {
    let label_width = rows
        .iter()
        .map(|row| UnicodeWidthStr::width(row.label))
        .max()
        .unwrap_or(0)
        .max(8);
    let widths: Vec<_> = headers
        .iter()
        .enumerate()
        .map(|(index, name)| {
            rows.iter()
                .map(|row| row.values.get(index).map_or(0, |cell| cell_width(cell)))
                .max()
                .unwrap_or(0)
                .max(UnicodeWidthStr::width(name.as_str()))
        })
        .collect();
    let required = label_width + widths.iter().sum::<usize>() + 2 * headers.len();
    (label_width, widths, required)
}

fn grid(
    lines: &mut Vec<Line<'static>>,
    headers: &[String],
    rows: &[MetricRow],
    width: u16,
    theme: Theme,
) -> bool {
    let (label_width, widths, required) = grid_width(headers, rows);
    if headers.is_empty() || required > usize::from(width) {
        return false;
    }
    let palette = theme.palette();
    let mut header = vec![Span::styled(
        format!("{:label_width$}", "Metric"),
        Style::default().fg(palette.muted),
    )];
    for (name, cell_width) in headers.iter().zip(&widths) {
        header.push(Span::raw("  "));
        let padding = " ".repeat(cell_width.saturating_sub(UnicodeWidthStr::width(name.as_str())));
        if !name.starts_with("% of ") {
            header.push(Span::raw(padding.clone()));
        }
        header.push(Span::styled(
            name.clone(),
            Style::default()
                .fg(palette.title)
                .add_modifier(Modifier::BOLD),
        ));
        if name.starts_with("% of ") {
            header.push(Span::raw(padding));
        }
    }
    lines.push(Line::from(header).style(theme.base_style()));
    for row in rows {
        let mut spans = vec![Span::styled(row.label, Style::default().fg(row.color))];
        spans.push(Span::raw(
            " ".repeat(label_width - UnicodeWidthStr::width(row.label)),
        ));
        for ((cell, cell_width), name) in row.values.iter().zip(&widths).zip(headers) {
            spans.push(Span::raw("  "));
            let padding = " ".repeat(cell_width.saturating_sub(self::cell_width(cell)));
            if !name.starts_with("% of ") {
                spans.push(Span::raw(padding.clone()));
            }
            spans.extend(cell.iter().cloned());
            if name.starts_with("% of ") {
                spans.push(Span::raw(padding));
            }
        }
        lines.push(Line::from(spans).style(theme.base_style()));
    }
    true
}

fn cards(lines: &mut Vec<Line<'static>>, headers: &[String], rows: &[MetricRow], theme: Theme) {
    for (index, name) in headers.iter().enumerate() {
        heading(lines, name, theme);
        for row in rows {
            lines.push(
                field_line(row.label, row.values[index].clone(), row.color)
                    .style(theme.base_style()),
            );
        }
    }
}

const CATEGORY_LABELS: [&str; 6] = [
    "Uncached input",
    "Cache read",
    "Cache write",
    "Non-reasoning output",
    "Reasoning output",
    "Unclassified",
];

fn category_colors(theme: Theme) -> [Color; 6] {
    let palette = theme.palette();
    let (blue, orange, violet) = match theme {
        Theme::Dark => (
            Color::Rgb(96, 165, 250),
            Color::Rgb(251, 146, 60),
            Color::Rgb(192, 132, 252),
        ),
        Theme::Light => (
            Color::Rgb(29, 78, 216),
            Color::Rgb(180, 83, 9),
            Color::Rgb(126, 34, 206),
        ),
    };
    [
        blue,
        palette.success,
        palette.accent,
        orange,
        violet,
        palette.muted,
    ]
}

fn category_counts(usage: TokenUsage) -> Option<[u64; 6]> {
    if !usage.has_valid_breakdown() {
        return None;
    }
    let counts = [
        usage
            .input_tokens
            .checked_sub(usage.cached_input_tokens)?
            .checked_sub(usage.cache_write_input_tokens)?,
        usage.cached_input_tokens,
        usage.cache_write_input_tokens,
        usage
            .output_tokens
            .checked_sub(usage.reasoning_output_tokens)?,
        usage.reasoning_output_tokens,
        usage.unclassified(),
    ];
    (counts.iter().map(|&count| u128::from(count)).sum::<u128>() == u128::from(usage.total_tokens))
        .then_some(counts)
}

fn recorded_category(usage: TokenUsage, category: usize) -> Option<u64> {
    match category {
        0 => usage
            .input_tokens
            .checked_sub(usage.cached_input_tokens)?
            .checked_sub(usage.cache_write_input_tokens),
        1 => Some(usage.cached_input_tokens),
        2 => Some(usage.cache_write_input_tokens),
        3 => usage
            .output_tokens
            .checked_sub(usage.reasoning_output_tokens),
        4 => Some(usage.reasoning_output_tokens),
        _ => Some(usage.unclassified()),
    }
}

/// Round to an eighth of a terminal cell, never invent a whole cell for a
/// nonzero category. Numeric proportions remain visible below that resolution.
fn fraction_bar(
    numerator: u64,
    denominator: u64,
    cells: usize,
    fill: Color,
    track: Color,
) -> Vec<Span<'static>> {
    if denominator == 0 || cells == 0 {
        return Vec::new();
    }
    let units = ((u128::from(numerator.min(denominator)) * cells as u128 * 8
        + u128::from(denominator) / 2)
        / u128::from(denominator)) as usize;
    let full = units / 8;
    let partial = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"][units % 8];
    let mut spans = vec![Span::styled("█".repeat(full), Style::default().fg(fill))];
    if !partial.is_empty() {
        spans.push(Span::styled(partial, Style::default().fg(fill)));
    }
    spans.push(Span::styled(
        "░".repeat(cells.saturating_sub(full + usize::from(!partial.is_empty()))),
        Style::default().fg(track),
    ));
    spans
}

fn category_value(
    usage: TokenUsage,
    category: usize,
    ratio: bool,
    cells: usize,
    color: Color,
    theme: Theme,
) -> Vec<Span<'static>> {
    let count = if category_counts(usage).is_some() {
        recorded_category(usage, category)
    } else {
        // Keep source counts, but never label an invalid subtraction as a
        // mutually exclusive count even when saturating_sub would return zero.
        match category {
            0 | 3 => None,
            _ => recorded_category(usage, category),
        }
    };
    let mut spans = value(count.map_or_else(|| "unavailable".into(), number), color);
    if ratio && category_counts(usage).is_some() {
        let count = count.unwrap_or(0);
        spans.push(Span::styled(
            format!("  {}", percentage(count, usage.total_tokens)),
            Style::default().fg(theme.palette().muted),
        ));
        if usage.total_tokens > 0 && cells > 0 {
            spans.push(Span::raw(" "));
            spans.extend(fraction_bar(
                count,
                usage.total_tokens,
                cells,
                color,
                theme.palette().gauge_track,
            ));
        }
    }
    spans
}

fn token_rows(
    columns: &[(String, TokenUsage)],
    ratio_column: Option<usize>,
    cells: usize,
    theme: Theme,
) -> Vec<MetricRow> {
    let colors = category_colors(theme);
    let mut rows = vec![MetricRow {
        label: "Total tokens",
        values: columns
            .iter()
            .map(|(_, usage)| value(number(usage.total_tokens), theme.palette().title))
            .collect(),
        color: theme.palette().title,
    }];
    for category in 0..6 {
        if category == 5 && columns.iter().all(|(_, usage)| usage.unclassified() == 0) {
            continue;
        }
        rows.push(MetricRow {
            label: CATEGORY_LABELS[category],
            values: columns
                .iter()
                .enumerate()
                .map(|(index, (_, usage))| {
                    category_value(
                        *usage,
                        category,
                        ratio_column == Some(index),
                        cells,
                        colors[category],
                        theme,
                    )
                })
                .collect(),
            color: colors[category],
        });
    }
    rows
}

fn breakdown_notes(lines: &mut Vec<Line<'static>>, columns: &[(String, TokenUsage)], theme: Theme) {
    for (name, usage) in columns {
        if category_counts(*usage).is_none() {
            lines.push(Line::styled(
                format!(
                    "{}: breakdown unavailable (overlapping cache subsets or inconsistent totals)",
                    terminal_safe_text(name)
                ),
                Style::default().fg(theme.palette().warning),
            ));
            note(
                lines,
                format!(
                    "Recorded input {}; cache read {}; cache write {}; output {}; reasoning {}; unclassified {}.",
                    number(usage.input_tokens),
                    number(usage.cached_input_tokens),
                    number(usage.cache_write_input_tokens),
                    number(usage.output_tokens),
                    number(usage.reasoning_output_tokens),
                    number(usage.unclassified())
                ),
                theme,
            );
        } else if usage.total_tokens == 0 {
            note(
                lines,
                format!(
                    "{}: All ratios unavailable (zero denominator); no observed tokens.",
                    terminal_safe_text(name)
                ),
                theme,
            );
        }
    }
}

fn token_comparison(
    lines: &mut Vec<Line<'static>>,
    columns: &[(String, TokenUsage)],
    show_composition: bool,
    width: u16,
    theme: Theme,
) {
    heading(lines, "Token counts", theme);
    if columns.is_empty() {
        note(lines, "No token columns available.", theme);
        return;
    }
    let focus = show_composition.then(|| {
        columns
            .iter()
            .position(|(name, _)| name == "Total")
            .unwrap_or(columns.len() - 1)
    });
    let mut names: Vec<_> = columns
        .iter()
        .map(|(name, _)| terminal_safe_text(name))
        .collect();
    if let Some(index) = focus {
        names.push(format!("% of {}", terminal_safe_text(&columns[index].0)));
    }
    let comparison_rows = |cells| {
        let mut rows = token_rows(columns, None, 0, theme);
        if let Some(index) = focus {
            let usage = columns[index].1;
            let colors = category_colors(theme);
            let percent_width = category_counts(usage)
                .map(|counts| {
                    counts
                        .iter()
                        .map(|&count| percentage(count, usage.total_tokens).len())
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            for row in &mut rows {
                let ratio = CATEGORY_LABELS
                    .iter()
                    .position(|label| *label == row.label)
                    .map_or_else(Vec::new, |category| {
                        if let Some(counts) = category_counts(usage) {
                            let mut spans = vec![Span::styled(
                                format!(
                                    "{:>percent_width$}",
                                    percentage(counts[category], usage.total_tokens)
                                ),
                                Style::default().fg(theme.palette().muted),
                            )];
                            if usage.total_tokens > 0 && cells > 0 {
                                spans.push(Span::raw(" "));
                                spans.extend(fraction_bar(
                                    counts[category],
                                    usage.total_tokens,
                                    cells,
                                    colors[category],
                                    theme.palette().gauge_track,
                                ));
                            }
                            spans
                        } else {
                            value("unavailable", theme.palette().muted)
                        }
                    });
                row.values.push(ratio);
            }
        }
        rows
    };
    let mut rows = comparison_rows(0);
    let (_, _, required) = grid_width(&names, &rows);
    if required <= usize::from(width) {
        let cells = usize::from(width).saturating_sub(required + 1).min(40);
        if cells >= 8 && focus.is_some() {
            rows = comparison_rows(cells);
        }
        grid(lines, &names, &rows, width, theme);
    } else {
        for (name, usage) in columns {
            heading(lines, name, theme);
            let single = vec![(name.clone(), *usage)];
            let rows = token_rows(&single, show_composition.then_some(0), 0, theme);
            for row in rows {
                lines.push(field_line(row.label, row.values[0].clone(), row.color));
            }
        }
    }
    if show_composition {
        note(
            lines,
            "Categories add to total tokens; all percentages use total tokens.",
            theme,
        );
    }
    breakdown_notes(lines, columns, theme);
}

fn token_lines(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    usage: TokenUsage,
    width: u16,
    theme: Theme,
) {
    lines.push(field_line(
        label,
        value(number(usage.total_tokens), theme.palette().title),
        theme.palette().foreground,
    ));
    let colors = category_colors(theme);
    for category in 0..6 {
        if category == 5 && usage.unclassified() == 0 {
            continue;
        }
        let base = category_value(usage, category, true, 0, colors[category], theme);
        let available = usize::from(width).saturating_sub(
            UnicodeWidthStr::width(CATEGORY_LABELS[category]) + 2 + cell_width(&base) + 1,
        );
        let cells = if available >= 8 { available.min(40) } else { 0 };
        lines.push(field_line(
            CATEGORY_LABELS[category],
            category_value(usage, category, true, cells, colors[category], theme),
            colors[category],
        ));
    }
    note(
        lines,
        "Categories add to total tokens; all percentages use total tokens.",
        theme,
    );
    breakdown_notes(lines, &[(label.into(), usage)], theme);
}

fn quota_comparison(
    lines: &mut Vec<Line<'static>>,
    columns: &[(String, WindowUsage)],
    account_used_percent: f64,
    long_context: bool,
    width: u16,
    theme: Theme,
) {
    let palette = theme.palette();
    heading(lines, "Quota estimate", theme);
    let gauge = if account_used_percent.is_finite() && account_used_percent >= 0.0 {
        format!("{account_used_percent:.2}%")
    } else {
        "unavailable".into()
    };
    lines.push(field_line(
        "Account gauge used",
        value(gauge, palette.warning),
        palette.foreground,
    ));
    let headers: Vec<_> = columns
        .iter()
        .map(|(name, _)| terminal_safe_text(name))
        .collect();
    let rows = vec![
        MetricRow {
            label: "TOKEN%",
            values: columns
                .iter()
                .map(|(_, usage)| {
                    value(
                        if usage.local_token_share_percent.is_finite() {
                            format!("{:.4}%", usage.local_token_share_percent)
                        } else {
                            "unavailable".into()
                        },
                        palette.accent,
                    )
                })
                .collect(),
            color: palette.foreground,
        },
        MetricRow {
            label: "Estimated quota",
            values: columns
                .iter()
                .map(|(_, usage)| {
                    value(
                        if usage.estimated_quota_percent.is_finite() {
                            format_estimated_quota(
                                usage.estimated_quota_percent,
                                usage.quota_confidence,
                            )
                        } else {
                            "-".into()
                        },
                        if usage.quota_confidence == Confidence::Unknown {
                            palette.muted
                        } else {
                            palette.warning
                        },
                    )
                })
                .collect(),
            color: palette.foreground,
        },
        MetricRow {
            label: "Confidence",
            values: columns
                .iter()
                .map(|(_, usage)| value(format!("{:?}", usage.quota_confidence), palette.muted))
                .collect(),
            color: palette.foreground,
        },
    ];
    if !grid(lines, &headers, &rows, width, theme) {
        cards(lines, &headers, &rows, theme);
    }
    note(
        lines,
        "TOKEN%: share of all observed cycle tokens. EST: account gauge split by credit weights.",
        theme,
    );
    note(
        lines,
        format!(
            "Longx: {} for credit weighting; estimates are not official session billing.",
            if long_context { "on" } else { "off" }
        ),
        theme,
    );
}

fn cost_formatted(amount: ApiCostAmount, state: ApiCostWindowState) -> String {
    if amount.observed_samples == 0 && amount.observed_tokens == 0 {
        "-".into()
    } else {
        format_scoped_api_cost_amount(state, amount)
    }
}

fn cost_color(amount: ApiCostAmount, state: ApiCostWindowState, theme: Theme) -> Color {
    if cost_formatted(amount, state) == "-" {
        theme.palette().muted
    } else if state == ApiCostWindowState::Incomplete
        || !amount.range_is_exact()
        || amount.priced_tokens < amount.observed_tokens
        || amount.priced_samples < amount.observed_samples
    {
        theme.palette().warning
    } else {
        theme.palette().success
    }
}

fn cost_rows(
    columns: &[(String, ApiCostAmount)],
    state: ApiCostWindowState,
    theme: Theme,
) -> Vec<MetricRow> {
    let palette = theme.palette();
    let labels = [
        "API equivalent",
        "Priced token coverage",
        "Priced / observed tokens",
        "Priced / observed samples",
        "Unpriced tokens",
        "Unpriced samples",
    ];
    labels
        .iter()
        .enumerate()
        .map(|(row, &label)| MetricRow {
            label,
            values: columns
                .iter()
                .map(|(_, amount)| {
                    let text = match row {
                        0 => cost_formatted(*amount, state),
                        1 => {
                            if amount.observed_tokens == 0 {
                                "unavailable".into()
                            } else {
                                percentage(amount.priced_tokens, amount.observed_tokens)
                            }
                        }
                        2 => format!(
                            "{} / {}",
                            number(amount.priced_tokens),
                            number(amount.observed_tokens)
                        ),
                        3 => format!(
                            "{} / {}",
                            number(amount.priced_samples),
                            number(amount.observed_samples)
                        ),
                        4 => number(amount.observed_tokens.saturating_sub(amount.priced_tokens)),
                        _ => number(
                            amount
                                .observed_samples
                                .saturating_sub(amount.priced_samples),
                        ),
                    };
                    value(
                        text,
                        if row <= 1 {
                            cost_color(*amount, state, theme)
                        } else {
                            palette.foreground
                        },
                    )
                })
                .collect(),
            color: palette.foreground,
        })
        .collect()
}

fn cost_notes(
    lines: &mut Vec<Line<'static>>,
    columns: &[(String, ApiCostAmount)],
    state: ApiCostWindowState,
    theme: Theme,
) {
    let no_samples: Vec<_> = columns
        .iter()
        .filter(|(_, amount)| amount.observed_samples == 0)
        .map(|(name, _)| terminal_safe_text(name))
        .collect();
    if !no_samples.is_empty() {
        note(
            lines,
            format!("{}: No observed samples.", no_samples.join(", ")),
            theme,
        );
    }
    if columns.iter().any(|(_, amount)| {
        amount.priced_tokens < amount.observed_tokens
            || amount.priced_samples < amount.observed_samples
    }) {
        note(
            lines,
            "Partial usage is unpriced; unpriced tokens and samples are excluded from the subtotal.",
            theme,
        );
    }
    match state {
        ApiCostWindowState::Incomplete => note(
            lines,
            "Window incomplete; any displayed subtotal is a known lower bound.",
            theme,
        ),
        ApiCostWindowState::Unavailable => note(
            lines,
            "API-equivalent cost unavailable for this window.",
            theme,
        ),
        ApiCostWindowState::NoLocalData => {
            note(lines, "No local pricing evidence for this window.", theme)
        }
        ApiCostWindowState::Complete => {}
    }
    if columns
        .iter()
        .any(|(_, amount)| amount.has_priced_usage() && !amount.range_is_exact())
    {
        note(
            lines,
            "Price ranges reflect unresolved short / long-context request attribution.",
            theme,
        );
    }
    note(
        lines,
        "Model tokens only; not the subscription bill or tool fees. Samples are usage records, not request counts.",
        theme,
    );
}

fn cost_comparison(
    lines: &mut Vec<Line<'static>>,
    columns: &[(String, ApiCostAmount)],
    state: ApiCostWindowState,
    width: u16,
    theme: Theme,
) {
    heading(lines, "API-equivalent cost", theme);
    let headers: Vec<_> = columns
        .iter()
        .map(|(name, _)| terminal_safe_text(name))
        .collect();
    let rows = cost_rows(columns, state, theme);
    if !grid(lines, &headers, &rows, width, theme) {
        cards(lines, &headers, &rows, theme);
    }
    cost_notes(lines, columns, state, theme);
}

fn cost_lines(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    amount: ApiCostAmount,
    state: ApiCostWindowState,
    _width: u16,
    theme: Theme,
) {
    lines.push(field_line(
        &format!("{prefix} API equivalent"),
        value(
            cost_formatted(amount, state),
            cost_color(amount, state, theme),
        ),
        theme.palette().foreground,
    ));
    for row in cost_rows(&[(prefix.into(), amount)], state, theme)
        .into_iter()
        .skip(1)
    {
        lines.push(field_line(row.label, row.values[0].clone(), row.color));
    }
    cost_notes(lines, &[(prefix.into(), amount)], state, theme);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage() -> TokenUsage {
        TokenUsage {
            input_tokens: 800,
            cached_input_tokens: 600,
            cache_write_input_tokens: 20,
            output_tokens: 200,
            reasoning_output_tokens: 150,
            unclassified_tokens: 200,
            total_tokens: 1_200,
        }
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn compact_text(lines: &[Line<'_>]) -> String {
        text(lines).replace('\n', "")
    }

    fn span_range(line: &Line<'_>, content: &str) -> std::ops::Range<usize> {
        let mut column = 0;
        for span in &line.spans {
            let end = column + span.width();
            if span.content == content {
                return column..end;
            }
            column = end;
        }
        panic!(
            "missing span {content:?} in {}",
            text(std::slice::from_ref(line))
        );
    }

    fn columns() -> Vec<(String, TokenUsage)> {
        let own = TokenUsage {
            input_tokens: 63_660_760,
            cached_input_tokens: 61_897_856,
            output_tokens: 289_930,
            reasoning_output_tokens: 126_242,
            total_tokens: 63_950_690,
            ..TokenUsage::default()
        };
        let delegated = TokenUsage {
            input_tokens: 67_641_776,
            cached_input_tokens: 65_154_688,
            output_tokens: 366_457,
            reasoning_output_tokens: 143_255,
            total_tokens: 68_008_233,
            ..TokenUsage::default()
        };
        let mut total = own;
        total.add_assign(delegated);
        vec![
            ("Own".into(), own),
            ("Delegated".into(), delegated),
            ("Total".into(), total),
        ]
    }

    #[test]
    fn usage_block_mutually_exclusive_categories_add_to_the_observed_total() {
        assert_eq!(category_counts(usage()), Some([180, 600, 20, 50, 150, 200]));
        for (_, usage) in columns() {
            let counts = category_counts(usage).unwrap();
            assert_eq!(
                counts.iter().map(|&value| u128::from(value)).sum::<u128>(),
                u128::from(usage.total_tokens)
            );
        }
        for theme in [Theme::Dark, Theme::Light] {
            let colors = category_colors(theme);
            for (index, color) in colors.iter().enumerate() {
                assert!(!colors[..index].contains(color));
            }
        }
    }

    #[test]
    fn usage_block_comparison_keeps_small_output_categories_visible_and_exact() {
        for theme in [Theme::Dark, Theme::Light] {
            let lines = UsageBlock::TokenComparison {
                columns: columns(),
                show_composition: true,
            }
            .render(137, theme);
            let output = text(&lines);
            assert!(lines.len() <= 12, "{output}");
            for marker in [
                "Own",
                "Delegated",
                "% of Total",
                "131,958,923",
                "127,052,544",
                "4,249,992",
                "386,890",
                "269,497",
                "96.28%",
                "0.29%",
                "0.20%",
            ] {
                assert!(output.contains(marker), "{marker}: {output}");
            }
            let header = &lines[1];
            let totals = &lines[2];
            let cache_read = lines
                .iter()
                .find(|line| text(std::slice::from_ref(line)).starts_with("Cache read"))
                .unwrap();
            for (label, total, cached) in [
                ("Own", "63,950,690", "61,897,856"),
                ("Delegated", "68,008,233", "65,154,688"),
                ("Total", "131,958,923", "127,052,544"),
            ] {
                let edge = span_range(header, label).end;
                assert_eq!(span_range(totals, total).end, edge);
                assert_eq!(span_range(cache_read, cached).end, edge);
            }
            assert!(span_range(header, "% of Total").start > span_range(header, "Total").end);
            assert_eq!(
                span_range(cache_read, "96.28%").start,
                span_range(header, "% of Total").start,
            );
            for category in ["Non-reasoning output", "Reasoning output"] {
                let line = lines
                    .iter()
                    .find(|line| text(std::slice::from_ref(line)).starts_with(category))
                    .unwrap();
                assert!(
                    line.spans.iter().any(|span| span.content.contains('▏')),
                    "small category uses a fractional cell: {}",
                    text(std::slice::from_ref(line))
                );
                assert!(
                    !line.spans.iter().any(|span| span.content.contains('█')),
                    "small category must not be inflated to a whole cell"
                );
            }
            assert!(lines.iter().all(|line| line.width() <= 137));
            let counts = UsageBlock::TokenComparison {
                columns: columns(),
                show_composition: false,
            }
            .render(137, theme);
            assert!(!text(&counts).contains("% of "));
            assert!(!text(&counts).contains('░'));
        }
    }

    #[test]
    fn usage_block_invalid_cache_overlap_keeps_source_counts_and_hides_derived_counts() {
        let invalid = TokenUsage {
            input_tokens: 100,
            cached_input_tokens: 80,
            cache_write_input_tokens: 40,
            output_tokens: 20,
            total_tokens: 120,
            ..TokenUsage::default()
        };
        assert!(
            invalid.has_valid_breakdown(),
            "the domain validator alone permits overlapping subsets"
        );
        assert!(category_counts(invalid).is_none());
        for theme in [Theme::Dark, Theme::Light] {
            let output = text(
                &UsageBlock::Tokens {
                    label: "Own tokens".into(),
                    usage: invalid,
                }
                .render(95, theme),
            );
            assert!(output.contains("Uncached input: unavailable"));
            assert!(output.contains("Non-reasoning output: unavailable"));
            assert!(output.contains("breakdown unavailable"));
            assert!(output.contains("cache read 80; cache write 40"));
            assert!(!output.contains('░'));
        }
    }

    #[test]
    fn usage_block_zero_missing_input_and_u64_boundaries_remain_explicit() {
        let only_total = TokenUsage {
            total_tokens: 100,
            ..TokenUsage::default()
        };
        assert_eq!(category_counts(only_total), Some([0, 0, 0, 0, 0, 100]));
        let output_only = TokenUsage {
            output_tokens: 7,
            reasoning_output_tokens: 3,
            total_tokens: 7,
            ..TokenUsage::default()
        };
        assert_eq!(category_counts(output_only), Some([0, 0, 0, 4, 3, 0]));
        let max = TokenUsage {
            input_tokens: u64::MAX - 2,
            cached_input_tokens: u64::MAX - 3,
            output_tokens: 1,
            reasoning_output_tokens: 1,
            unclassified_tokens: 1,
            total_tokens: u64::MAX,
            ..TokenUsage::default()
        };
        assert_eq!(category_counts(max), Some([1, u64::MAX - 3, 0, 0, 1, 1]));
        assert_eq!(number(u64::MAX), "18,446,744,073,709,551,615");
        for theme in [Theme::Dark, Theme::Light] {
            let zero = text(
                &UsageBlock::Tokens {
                    label: "Zero".into(),
                    usage: TokenUsage::default(),
                }
                .render(95, theme),
            );
            assert!(zero.contains("no observed tokens"));
            assert!(zero.contains("unavailable (zero denominator)"));
            assert!(!zero.contains("0.00%"));
            for width in [1, 2, 8, 17, 32, 60, 95, 137] {
                let lines = UsageBlock::TokenComparison {
                    columns: vec![
                        ("Own".into(), max),
                        ("Delegated".into(), output_only),
                        ("Total".into(), max),
                    ],
                    show_composition: true,
                }
                .render(width, theme);
                assert!(
                    lines.iter().all(|line| line.width() <= usize::from(width)),
                    "width {width}: {}",
                    text(&lines)
                );
                assert!(compact_text(&lines).contains("18,446,744,073,709,551,615"));
                assert!(compact_text(&lines).contains("<0.01%"));
            }
        }
    }

    #[test]
    fn usage_block_fractional_bars_have_bounded_width_and_do_not_overflow() {
        for cells in [1, 8, 40] {
            for (value, denominator) in [
                (0, 100),
                (1, 100),
                (100, 100),
                (u64::MAX, u64::MAX),
                (1, u64::MAX),
            ] {
                let spans = fraction_bar(value, denominator, cells, Color::Green, Color::Gray);
                assert_eq!(cell_width(&spans), cells);
            }
        }
        assert!(fraction_bar(1, 0, 40, Color::Green, Color::Gray).is_empty());
        let spans = fraction_bar(1, 200, 40, Color::Green, Color::Gray);
        assert!(spans.iter().any(|span| span.content.as_ref() == "▎"));
        assert!(!spans.iter().any(|span| span.content.contains('█')));
    }

    #[test]
    fn usage_block_unicode_headers_align_and_narrow_cards_preserve_all_digits() {
        for theme in [Theme::Dark, Theme::Light] {
            for width in [2, 8, 17, 32, 60, 95, 137] {
                let lines = UsageBlock::TokenComparison {
                    columns: vec![
                        ("自身 👩‍💻".into(), usage()),
                        ("委派".into(), usage()),
                        ("Total".into(), usage()),
                    ],
                    show_composition: true,
                }
                .render(width, theme);
                assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
                let output = compact_text(&lines);
                for marker in ["自身 👩‍💻", "委派", "1,200", "600", "180", "150", "200"]
                {
                    assert!(
                        output.contains(marker),
                        "{marker} at width {width}: {output}"
                    );
                }
            }
            let lines = UsageBlock::TokenComparison {
                columns: vec![
                    ("自身 👩‍💻".into(), usage()),
                    ("委派".into(), usage()),
                    ("Total".into(), usage()),
                ],
                show_composition: true,
            }
            .render(137, theme);
            let header_width = lines[1].width();
            assert!(
                lines[2..]
                    .iter()
                    .filter(|line| !line.spans.is_empty()
                        && CATEGORY_LABELS
                            .iter()
                            .any(|label| line.spans[0].content.as_ref() == *label))
                    .all(|line| line.width() == header_width)
            );
        }
        assert!(
            UsageBlock::Tokens {
                label: "Tokens".into(),
                usage: usage()
            }
            .render(0, Theme::Dark)
            .is_empty()
        );
    }

    fn cost() -> ApiCostAmount {
        ApiCostAmount {
            minimum_pico_usd: crate::domain::PicoUsd::new(1_234_500_000_000),
            maximum_pico_usd: crate::domain::PicoUsd::new(2_345_600_000_000),
            observed_tokens: 1_200,
            priced_tokens: 1_100,
            observed_samples: 3,
            priced_samples: 2,
        }
    }

    #[test]
    fn usage_block_api_cost_explains_unpriced_usage_window_partial_and_ranges() {
        for theme in [Theme::Dark, Theme::Light] {
            for width in [1, 8, 32, 95, 137] {
                let lines = UsageBlock::CostComparison {
                    columns: vec![("Own".into(), cost()), ("Total".into(), cost())],
                    state: ApiCostWindowState::Incomplete,
                }
                .render(width, theme);
                assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
                let output = compact_text(&lines);
                for marker in [
                    "$1.2345–$2.3456+",
                    "91.67%",
                    "1,100 / 1,200",
                    "2 / 3",
                    "Unpriced tokens",
                    "Unpriced samples",
                    "Partial usage is unpriced",
                    "Window incomplete",
                    "Price ranges",
                    "not request counts",
                ] {
                    assert!(output.contains(marker), "missing {marker}: {output}");
                }
                assert!(
                    !output.contains('░'),
                    "coverage stays numeric rather than a coarse short bar"
                );
            }
            let full = ApiCostAmount {
                minimum_pico_usd: crate::domain::PicoUsd::new(1_000_000_000_000),
                maximum_pico_usd: crate::domain::PicoUsd::new(1_000_000_000_000),
                observed_tokens: 100,
                priced_tokens: 100,
                observed_samples: 1,
                priced_samples: 1,
            };
            let output = text(
                &UsageBlock::Cost {
                    prefix: "Own".into(),
                    amount: full,
                    state: ApiCostWindowState::Incomplete,
                }
                .render(95, theme),
            );
            assert!(output.contains("$1.0000+"));
            assert!(output.contains("Window incomplete"));
            assert!(!output.contains("Partial usage is unpriced"));
        }
    }

    #[test]
    fn usage_block_cost_distinguishes_missing_evidence_from_a_priced_zero_sample() {
        for theme in [Theme::Dark, Theme::Light] {
            let default = text(
                &UsageBlock::Cost {
                    prefix: "Own".into(),
                    amount: ApiCostAmount::default(),
                    state: ApiCostWindowState::Complete,
                }
                .render(95, theme),
            );
            assert!(default.contains("Own API equivalent: -"));
            assert!(default.contains("No observed samples"));
            assert!(!default.contains("$0.0000"));
            let zero = ApiCostAmount {
                observed_samples: 1,
                priced_samples: 1,
                ..ApiCostAmount::default()
            };
            let output = text(
                &UsageBlock::Cost {
                    prefix: "Own".into(),
                    amount: zero,
                    state: ApiCostWindowState::Complete,
                }
                .render(95, theme),
            );
            assert!(output.contains("Own API equivalent: $0.0000"));
            assert!(!output.contains("No observed samples"));
            assert!(output.contains("Priced token coverage: unavailable"));
            for state in [
                ApiCostWindowState::Unavailable,
                ApiCostWindowState::NoLocalData,
            ] {
                assert!(
                    text(
                        &UsageBlock::Cost {
                            prefix: "Own".into(),
                            amount: cost(),
                            state
                        }
                        .render(95, theme)
                    )
                    .contains("Own API equivalent: -")
                );
            }
        }
    }

    #[test]
    fn usage_block_quota_keeps_account_gauge_token_denominator_and_credit_estimate_separate() {
        let own = WindowUsage {
            local_token_share_percent: 12.34567,
            estimated_quota_percent: 4.56,
            quota_confidence: Confidence::Low,
            ..WindowUsage::default()
        };
        for theme in [Theme::Dark, Theme::Light] {
            for width in [1, 8, 32, 95, 137] {
                let lines = UsageBlock::QuotaComparison {
                    columns: vec![
                        ("Own".into(), own),
                        ("Delegated".into(), WindowUsage::default()),
                        ("Total".into(), own),
                    ],
                    account_used_percent: 25.0,
                    long_context: true,
                }
                .render(width, theme);
                assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
                let output = compact_text(&lines);
                for marker in [
                    "Quota estimate",
                    "Account gauge used: 25.00%",
                    "TOKEN%",
                    "12.3457%",
                    "Estimated quota",
                    "~4.6%",
                    "Low",
                    "Unknown",
                    "observed cycle tokens",
                    "credit weights",
                    "Longx: on",
                ] {
                    assert!(output.contains(marker), "{marker}: {output}");
                }
            }
        }
    }
}
