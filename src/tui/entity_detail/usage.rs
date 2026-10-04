use super::*;

#[derive(Clone, Debug)]
pub(in crate::tui) enum UsageBlock {
    Tokens {
        label: String,
        usage: TokenUsage,
    },
    Scope {
        label: String,
        usage: WindowUsage,
        state: ApiCostWindowState,
    },
    Cost {
        prefix: String,
        amount: ApiCostAmount,
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
            Self::Tokens { label, usage } => {
                token_lines(&mut lines, label, *usage, width, theme);
            }
            Self::Scope {
                label,
                usage,
                state,
            } => {
                token_lines(
                    &mut lines,
                    &format!("{label} window tokens"),
                    usage.token_usage,
                    width,
                    theme,
                );
                let palette = theme.palette();
                pack(
                    &mut lines,
                    vec![
                        field(
                            "TOKEN%",
                            format!("{:.4}%", usage.local_token_share_percent),
                            palette.accent,
                            theme,
                        ),
                        field(
                            "Estimated quota",
                            format_estimated_quota(
                                usage.estimated_quota_percent,
                                usage.quota_confidence,
                            ),
                            if usage.quota_confidence == Confidence::Unknown {
                                palette.muted
                            } else {
                                palette.warning
                            },
                            theme,
                        ),
                        field(
                            "Quota confidence",
                            format!("{:?}", usage.quota_confidence),
                            palette.muted,
                            theme,
                        ),
                    ],
                    width,
                    theme,
                );
                cost_lines(
                    &mut lines,
                    label,
                    usage.api_equivalent_cost,
                    *state,
                    width,
                    theme,
                );
            }
            Self::Cost {
                prefix,
                amount,
                state,
            } => cost_lines(&mut lines, prefix, *amount, *state, width, theme),
        }
        // Keep every digit and label; narrow layouts use additional rows.
        wrapped_lines(&lines, width)
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
        format!("{:.2}%", numerator as f64 / denominator as f64 * 100.0)
    }
}

fn field(label: &str, value: String, color: Color, theme: Theme) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            format!("{}: ", terminal_safe_text(label)),
            Style::default().fg(theme.palette().foreground),
        ),
        Span::styled(
            value,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]
}

fn token_field(
    label: &str,
    value: u64,
    denominator: Option<(u64, &str)>,
    color: Color,
    width: u16,
    theme: Theme,
) -> Vec<Span<'static>> {
    let palette = theme.palette();
    let mut spans = vec![
        Span::styled(format!("{label} "), Style::default().fg(color)),
        Span::styled(
            number(value),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some((denominator, unit)) = denominator {
        let percent = percentage(value, denominator);
        spans.push(Span::styled(
            if denominator == 0 {
                format!(" · {percent}")
            } else {
                format!(" ({percent} {unit})")
            },
            Style::default().fg(palette.muted),
        ));
        // Subset bars remain independent of the total composition.
        if unit != "total" && denominator > 0 && width >= 70 {
            spans.push(Span::raw(" "));
            spans.extend(ratio_bar(value, denominator, 8, color, palette.gauge_track));
        }
    }
    spans
}

fn pack(lines: &mut Vec<Line<'static>>, items: Vec<Vec<Span<'static>>>, width: u16, theme: Theme) {
    let mut spans = Vec::new();
    let mut used = 0;
    for item in items {
        let item_width: usize = item.iter().map(Span::width).sum();
        if !spans.is_empty() && used + 2 + item_width > usize::from(width) {
            lines.push(Line::from(std::mem::take(&mut spans)).style(theme.base_style()));
            used = 0;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(
                "  ",
                Style::default().fg(theme.palette().muted),
            ));
            used += 2;
        }
        used += item_width;
        spans.extend(item);
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans).style(theme.base_style()));
    }
}

fn token_lines(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    usage: TokenUsage,
    width: u16,
    theme: Theme,
) {
    let palette = theme.palette();
    lines.push(
        Line::from(field(
            label,
            number(usage.total_tokens),
            palette.title,
            theme,
        ))
        .style(theme.base_style()),
    );
    if usage.is_zero() {
        pack(
            lines,
            vec![
                token_field("Input", 0, None, palette.accent, width, theme),
                token_field("Output", 0, None, palette.warning, width, theme),
                token_field("Unclassified", 0, None, palette.muted, width, theme),
                token_field("Cached input", 0, None, palette.success, width, theme),
                token_field("Cache write input", 0, None, palette.success, width, theme),
                token_field("Reasoning output", 0, None, palette.warning, width, theme),
            ],
            width,
            theme,
        );
        lines.push(Line::styled(
            "All ratios: unavailable (zero denominator)",
            Style::default().fg(palette.muted),
        ));
        return;
    }
    if usage.has_valid_breakdown() {
        let prefix = if width >= 24 { "Composition " } else { "" };
        let cells = usize::from(width)
            .saturating_sub(UnicodeWidthStr::width(prefix))
            .min(40);
        let mut spans = vec![Span::styled(prefix, Style::default().fg(palette.muted))];
        spans.extend(composition_bar(usage, cells, theme));
        lines.push(Line::from(spans).style(theme.base_style()));
    } else {
        lines.push(Line::styled(
            "Composition unavailable (inconsistent token breakdown)",
            Style::default().fg(palette.warning),
        ));
    }
    pack(
        lines,
        vec![
            token_field(
                "Input",
                usage.input_tokens,
                Some((usage.total_tokens, "total")),
                palette.accent,
                width,
                theme,
            ),
            token_field(
                "Output",
                usage.output_tokens,
                Some((usage.total_tokens, "total")),
                palette.warning,
                width,
                theme,
            ),
            token_field(
                "Unclassified",
                usage.unclassified(),
                None,
                palette.muted,
                width,
                theme,
            ),
        ],
        width,
        theme,
    );
    pack(
        lines,
        vec![
            token_field(
                "Cached input",
                usage.cached_input_tokens,
                Some((usage.input_tokens, "input")),
                palette.success,
                width,
                theme,
            ),
            token_field(
                "Cache write input",
                usage.cache_write_input_tokens,
                Some((usage.input_tokens, "input")),
                palette.success,
                width,
                theme,
            ),
        ],
        width,
        theme,
    );
    pack(
        lines,
        vec![token_field(
            "Reasoning output",
            usage.reasoning_output_tokens,
            Some((usage.output_tokens, "output")),
            palette.warning,
            width,
            theme,
        )],
        width,
        theme,
    );
}

/// Apportion only additive input/output/unclassified counts to the total bar.
/// Integer arithmetic keeps even u64::MAX totals and narrow bars exact.
fn composition_bar(usage: TokenUsage, cells: usize, theme: Theme) -> Vec<Span<'static>> {
    if usage.total_tokens == 0 || cells == 0 {
        return Vec::new();
    }
    let total = u128::from(usage.total_tokens);
    let components = [
        usage.input_tokens,
        usage.output_tokens,
        usage.unclassified(),
    ];
    let mut lengths =
        components.map(|tokens| (u128::from(tokens) * cells as u128 / total) as usize);
    let remainders = components.map(|tokens| u128::from(tokens) * cells as u128 % total);
    let mut order = [0, 1, 2];
    order.sort_by_key(|index| std::cmp::Reverse(remainders[*index]));
    for index in order
        .into_iter()
        .take(cells.saturating_sub(lengths.iter().sum()))
    {
        lengths[index] += 1;
    }
    let palette = theme.palette();
    lengths
        .into_iter()
        .zip([palette.accent, palette.warning, palette.muted])
        .filter(|(length, _)| *length > 0)
        .map(|(length, color)| Span::styled("█".repeat(length), Style::default().fg(color)))
        .collect()
}

fn ratio_bar(
    value: u64,
    denominator: u64,
    cells: usize,
    fill: Color,
    track: Color,
) -> Vec<Span<'static>> {
    let filled =
        (u128::from(value.min(denominator)) * cells as u128 / u128::from(denominator)) as usize;
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(fill)),
        Span::styled("░".repeat(cells - filled), Style::default().fg(track)),
    ]
}

fn cost_lines(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    amount: ApiCostAmount,
    state: ApiCostWindowState,
    width: u16,
    theme: Theme,
) {
    let palette = theme.palette();
    let formatted = format_scoped_api_cost_amount(state, amount);
    let color = if formatted == "-" {
        palette.muted
    } else if state == ApiCostWindowState::Incomplete
        || !amount.range_is_exact()
        || amount.priced_tokens < amount.observed_tokens
        || amount.priced_samples < amount.observed_samples
    {
        palette.warning
    } else {
        palette.success
    };
    lines.push(
        Line::from(field(
            &format!("{prefix} API equivalent"),
            formatted,
            color,
            theme,
        ))
        .style(theme.base_style()),
    );
    let mut coverage = field(
        "Priced token coverage",
        format!(
            "{} / {} ({})",
            number(amount.priced_tokens),
            number(amount.observed_tokens),
            percentage(amount.priced_tokens, amount.observed_tokens)
        ),
        color,
        theme,
    );
    if width >= 70 && amount.observed_tokens > 0 {
        coverage.push(Span::raw(" "));
        coverage.extend(ratio_bar(
            amount.priced_tokens,
            amount.observed_tokens,
            8,
            color,
            palette.gauge_track,
        ));
    }
    pack(
        lines,
        vec![
            coverage,
            field(
                "Usage samples",
                format!(
                    "{} observed; {} priced",
                    number(amount.observed_samples),
                    number(amount.priced_samples)
                ),
                palette.muted,
                theme,
            ),
        ],
        width,
        theme,
    );
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

    #[test]
    fn usage_block_preserves_exact_counts_and_all_five_ratios_in_compact_rows() {
        let lines = UsageBlock::Tokens {
            label: "Own cumulative tokens".into(),
            usage: usage(),
        }
        .render(95, Theme::Dark);
        let output = text(&lines);
        assert!(lines.len() <= 6, "{output}");
        for value in [
            "Own cumulative tokens: 1,200",
            "Input 800 (66.67% total)",
            "Output 200 (16.67% total)",
            "Unclassified 200",
            "Cached input 600 (75.00% input)",
            "Cache write input 20 (2.50% input)",
            "Reasoning output 150 (75.00% output)",
        ] {
            assert!(output.contains(value), "missing {value}: {output}");
        }
        assert_eq!(number(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn usage_block_composition_uses_only_additive_fields_and_fills_the_bar() {
        for theme in [Theme::Dark, Theme::Light] {
            let block = UsageBlock::Tokens {
                label: "Tokens".into(),
                usage: usage(),
            };
            let original = block.render(95, theme);
            let mut changed = usage();
            changed.cached_input_tokens = 0;
            changed.cache_write_input_tokens = 0;
            changed.reasoning_output_tokens = 0;
            let changed = UsageBlock::Tokens {
                label: "Tokens".into(),
                usage: changed,
            }
            .render(95, theme);
            assert_eq!(original[1], changed[1]);
            assert_eq!(
                original[1]
                    .spans
                    .iter()
                    .map(|span| span.content.matches('█').count())
                    .sum::<usize>(),
                40
            );
            assert_eq!(
                original[1]
                    .spans
                    .iter()
                    .filter(|span| span.content.contains('█'))
                    .count(),
                3
            );
        }
        let max = TokenUsage {
            input_tokens: u64::MAX - 2,
            output_tokens: 1,
            unclassified_tokens: 1,
            total_tokens: u64::MAX,
            ..TokenUsage::default()
        };
        assert_eq!(
            composition_bar(max, 7, Theme::Dark)
                .iter()
                .map(Span::width)
                .sum::<usize>(),
            7
        );
    }

    #[test]
    fn usage_block_zero_and_missing_denominators_are_explicit() {
        let lines = UsageBlock::Tokens {
            label: "Delegated cumulative tokens".into(),
            usage: TokenUsage::default(),
        }
        .render(95, Theme::Light);
        let output = text(&lines);
        assert!(lines.len() <= 5, "{output}");
        for value in [
            "Delegated cumulative tokens: 0",
            "Input 0",
            "Output 0",
            "Unclassified 0",
            "Cached input 0",
            "Cache write input 0",
            "Reasoning output 0",
            "All ratios: unavailable (zero denominator)",
            "unavailable (zero denominator)",
        ] {
            assert!(output.contains(value), "missing {value}: {output}");
        }
        assert!(!output.contains("0.00%"));
        let output = text(
            &UsageBlock::Tokens {
                label: "Unclassified".into(),
                usage: TokenUsage {
                    total_tokens: 100,
                    ..TokenUsage::default()
                },
            }
            .render(95, Theme::Dark),
        );
        assert!(output.contains("Unclassified 100"));
        assert!(output.contains("Cached input 0 · unavailable (zero denominator)"));
        assert!(output.contains("Reasoning output 0 · unavailable (zero denominator)"));
    }

    #[test]
    fn usage_block_narrow_rows_preserve_values_and_unicode_display_width() {
        for theme in [Theme::Dark, Theme::Light] {
            for width in [0, 1, 2, 8, 17, 32, 60, 95] {
                let lines = UsageBlock::Tokens {
                    label: "Tokens".into(),
                    usage: usage(),
                }
                .render(width, theme);
                assert!(
                    lines.iter().all(|line| line.width() <= usize::from(width)),
                    "width {width}: {}",
                    text(&lines)
                );
                if width > 0 {
                    assert!(text(&lines).replace('\n', "").contains("Tokens: 1,200"));
                } else {
                    assert!(lines.is_empty());
                }
            }
            for width in [2, 8, 17, 32, 95] {
                let lines = UsageBlock::Tokens {
                    label: "自身用量 🧪".into(),
                    usage: usage(),
                }
                .render(width, theme);
                assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
                assert!(
                    text(&lines)
                        .replace('\n', "")
                        .contains("自身用量 🧪: 1,200")
                );
            }
        }
    }

    #[test]
    fn usage_block_api_preserves_range_partial_unpriced_and_sample_semantics() {
        let amount = ApiCostAmount {
            minimum_pico_usd: crate::domain::PicoUsd::new(1_234_500_000_000),
            maximum_pico_usd: crate::domain::PicoUsd::new(2_345_600_000_000),
            observed_tokens: 1_200,
            priced_tokens: 1_100,
            observed_samples: 3,
            priced_samples: 2,
        };
        for width in [1, 8, 32, 95] {
            let lines = UsageBlock::Cost {
                prefix: "Own".into(),
                amount,
                state: ApiCostWindowState::Incomplete,
            }
            .render(width, Theme::Dark);
            assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
            let output = text(&lines).replace('\n', "");
            assert!(output.contains("Own API equivalent: $1.2345–$2.3456+"));
            assert!(output.contains("Priced token coverage: 1,100 / 1,200 (91.67%)"));
            assert!(output.contains("Usage samples: 3 observed; 2 priced"));
        }
        for state in [
            ApiCostWindowState::Unavailable,
            ApiCostWindowState::NoLocalData,
        ] {
            let output = text(
                &UsageBlock::Cost {
                    prefix: "Own".into(),
                    amount,
                    state,
                }
                .render(95, Theme::Light),
            );
            assert!(output.contains("Own API equivalent: -"));
        }
        let unpriced = ApiCostAmount {
            observed_samples: 1,
            observed_tokens: 100,
            ..ApiCostAmount::default()
        };
        let output = text(
            &UsageBlock::Cost {
                prefix: "Own".into(),
                amount: unpriced,
                state: ApiCostWindowState::Complete,
            }
            .render(95, Theme::Dark),
        );
        assert!(output.contains("Own API equivalent: -"));
        let output = text(
            &UsageBlock::Cost {
                prefix: "Own".into(),
                amount: ApiCostAmount::default(),
                state: ApiCostWindowState::Complete,
            }
            .render(95, Theme::Dark),
        );
        assert!(output.contains("Own API equivalent: $0.0000"));
        assert!(output.contains("unavailable (zero denominator)"));
    }

    #[test]
    fn usage_block_scope_preserves_token_share_estimate_and_unknown_confidence() {
        let usage = WindowUsage {
            token_usage: usage(),
            local_token_share_percent: 12.34567,
            estimated_quota_percent: 4.56,
            quota_confidence: Confidence::Low,
            ..WindowUsage::default()
        };
        let output = text(
            &UsageBlock::Scope {
                label: "Own".into(),
                usage,
                state: ApiCostWindowState::Complete,
            }
            .render(95, Theme::Dark),
        );
        for value in [
            "Own window tokens: 1,200",
            "TOKEN%: 12.3457%",
            "Estimated quota: ~4.6%",
            "Quota confidence: Low",
            "Own API equivalent:",
        ] {
            assert!(output.contains(value), "missing {value}: {output}");
        }
        let output = text(
            &UsageBlock::Scope {
                label: "Own".into(),
                usage: WindowUsage::default(),
                state: ApiCostWindowState::NoLocalData,
            }
            .render(95, Theme::Light),
        );
        assert!(output.contains("Estimated quota: -"));
        assert!(output.contains("Quota confidence: Unknown"));
        assert!(output.contains("Own API equivalent: -"));
    }
}
