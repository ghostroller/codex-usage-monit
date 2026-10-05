use super::super::entity_detail::DetailNode;
use super::testkit::{TuiHarness, gallery_directory};
use super::*;

const SIZES: [(u16, u16); 4] = [(120, 40), (80, 24), (60, 24), (32, 14)];
const MESSAGE_PREVIEW_SECTION: &str = "snapshot.message-preview";

fn detail_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    TuiHarness::from_snapshot(interaction_test_app(3, 2).snapshot, width, height, theme)
}

fn popup_text(app: &App) -> String {
    app.entity_detail
        .as_ref()
        .expect("details must be open")
        .lines
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

fn full_preview_message() -> String {
    let mut message = String::from(
        "请完整展示当前用户消息并保留多行段落。这里有中文标题、路径和编号，正文不能只显示截断预览。\n",
    );
    for index in 1..=24 {
        message.push_str(&format!(
            "第{index:02}段：逐项核对会话信息与精确数字，路径 /tmp/中文项目/消息.rs，Unicode 家庭 👨‍👩‍👧‍👦 和重音 e\u{301} 需要保留。\n"
        ));
    }
    message.push_str("终端控制字符：\u{1b}[31m安全内容\u{1b}[0m\t\r\u{202e}VISIBLE-SAFE-TEXT\n");
    message.push_str("完整中文多行消息末尾 PREVIEW-FULL-END");
    message
}

fn snapshot_preview(message: &str) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= 72 {
        normalized
    } else {
        format!("{}...", normalized.chars().take(69).collect::<String>())
    }
}

fn preview_turn_harness(width: u16, height: u16, theme: Theme, message: &str) -> TuiHarness {
    let mut harness = detail_harness(width, height, theme);
    harness.app.snapshot.turns[0].message_preview = Some(snapshot_preview(message));
    harness.app.local_snapshot.tasks.clear();
    harness.app.focus_turns();
    harness.render();
    harness
}

fn recorded_preview_messages(
    captured: DateTime<Utc>,
    turn_id: &str,
    message: &str,
) -> crate::session_details::SessionDetails {
    use crate::session_details::{DetailMessage, SessionDetails};

    let mut data = SessionDetails::default();
    data.files_read = 1;
    for (role, id, body) in [
        ("user", Some(turn_id), message),
        (
            "assistant",
            Some(turn_id),
            "ASSISTANT-CONTENT-MUST-STAY-HIDDEN",
        ),
        (
            "user",
            Some("other-turn"),
            "OTHER-TURN-CONTENT-MUST-STAY-HIDDEN",
        ),
        ("user", None, "UNASSIGNED-CONTENT-MUST-STAY-HIDDEN"),
        (
            "user",
            Some(turn_id),
            "<environment_context>CONTEXT-CONTENT-MUST-STAY-HIDDEN</environment_context>",
        ),
    ] {
        data.messages.push(DetailMessage {
            role: role.into(),
            text: body.into(),
            timestamp: Some(captured - ChronoDuration::seconds(1)),
            turn_id: id.map(str::to_owned),
            phase: None,
        });
    }
    // Native event and response records can repeat the same user message.
    // Its duplicate must not make the exact preview match ambiguous.
    data.messages.push(data.messages[0].clone());
    data
}

fn inject_preview_messages(harness: &mut TuiHarness, message: &str) {
    let data = recorded_preview_messages(
        harness.app.snapshot.as_of,
        &harness.app.snapshot.turns[0].turn_id,
        message,
    );
    let theme = harness.app.theme;
    harness
        .app
        .entity_detail
        .as_mut()
        .unwrap()
        .set_recorded_details(data, theme);
    harness.render();
}

fn assert_comparison_row(content: &str, label: &str, values: &[&str]) {
    let row = content
        .lines()
        .find(|line| line.contains(label))
        .unwrap_or_else(|| panic!("missing comparison row {label:?}: {content}"));
    let mut remainder = row;
    for value in values {
        let position = remainder
            .find(value)
            .unwrap_or_else(|| panic!("missing ordered value {value:?} in {row:?}"));
        remainder = &remainder[position + value.len()..];
    }
}

fn cycle_usage_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness = detail_harness(width, height, theme);
    set_task_parent(&mut harness.app, 1, 0);
    set_task_parent(&mut harness.app, 2, 1);
    for (task, total) in harness
        .app
        .snapshot
        .tasks
        .iter_mut()
        .zip([11_130, 22_290, 33_370])
    {
        task.token_usage = TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..TokenUsage::default()
        };
    }
    add_window_analysis(&mut harness.app, WindowScope::FiveHours, 1_113, 1.0);
    let threads = harness
        .app
        .snapshot
        .tasks
        .iter()
        .zip([1_113, 2_229, 3_337])
        .enumerate()
        .map(|(index, (task, total))| ThreadWindowUsage {
            thread_id: task.thread_id.clone(),
            usage: WindowUsage {
                token_usage: TokenUsage {
                    input_tokens: total,
                    total_tokens: total,
                    ..TokenUsage::default()
                },
                local_token_share_percent: index as f64 + 1.0,
                estimated_quota_percent: index as f64 + 0.5,
                quota_confidence: Confidence::Low,
                api_equivalent_cost: exact_api_cost((index as u128 + 1) * 1_000_000_000_000),
            },
        })
        .collect();
    harness.app.snapshot.window_analyses[0].threads = threads;
    // All values come from the injected snapshot, with no asynchronous reader.
    harness.app.local_snapshot.tasks.clear();
    harness.app.task_list_mode = TaskListMode::Tree;
    harness.app.selected_task = 0;
    harness.render();
    harness
}

fn screenshot_usage_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness = detail_harness(width, height, theme);
    set_task_parent(&mut harness.app, 1, 0);
    set_task_parent(&mut harness.app, 2, 1);
    let captured = DateTime::parse_from_rfc3339("2026-10-04T21:13:36Z")
        .unwrap()
        .with_timezone(&Utc);
    harness.app.snapshot.as_of = captured;
    // Preserve every known token component and Own pricing field from the
    // screenshot. The two descendant splits and delegated quota/cost are
    // synthetic fixture values, not recovered account billing data.
    let usages = [
        TokenUsage {
            input_tokens: 63_660_760,
            cached_input_tokens: 61_897_856,
            output_tokens: 289_930,
            reasoning_output_tokens: 126_242,
            total_tokens: 63_950_690,
            ..TokenUsage::default()
        },
        TokenUsage {
            input_tokens: 29_800_000,
            cached_input_tokens: 28_800_000,
            output_tokens: 200_000,
            reasoning_output_tokens: 80_000,
            total_tokens: 30_000_000,
            ..TokenUsage::default()
        },
        TokenUsage {
            input_tokens: 37_841_776,
            cached_input_tokens: 36_354_688,
            output_tokens: 166_457,
            reasoning_output_tokens: 63_255,
            total_tokens: 38_008_233,
            ..TokenUsage::default()
        },
    ];
    for (task, usage) in harness.app.snapshot.tasks.iter_mut().zip(usages) {
        task.token_usage = usage;
        task.created_at = Some(captured - ChronoDuration::hours(50));
        task.updated_at = Some(captured);
    }
    harness.app.snapshot.tasks[0].title = "Session usage comparison".into();
    add_window_analysis(&mut harness.app, WindowScope::Week, 63_950_690, 13.5638);
    let threads = harness
        .app
        .snapshot
        .tasks
        .iter()
        .zip(usages)
        .zip([8.1, 3.8, 4.9])
        .enumerate()
        .map(
            |(index, ((task, token_usage), estimated_quota_percent))| ThreadWindowUsage {
                thread_id: task.thread_id.clone(),
                usage: WindowUsage {
                    token_usage,
                    local_token_share_percent: token_usage.total_tokens as f64 / 63_950_690.0
                        * 13.5638,
                    estimated_quota_percent,
                    quota_confidence: Confidence::Low,
                    api_equivalent_cost: if index == 0 {
                        ApiCostAmount {
                            minimum_pico_usd: PicoUsd::new(24_642_300_000_000),
                            maximum_pico_usd: PicoUsd::new(24_642_300_000_000),
                            observed_samples: 517,
                            priced_samples: 501,
                            observed_tokens: 63_950_690,
                            priced_tokens: 62_890_536,
                        }
                    } else {
                        ApiCostAmount {
                            minimum_pico_usd: PicoUsd::new(
                                (index as u128 + 1) * 12_500_000_000_000,
                            ),
                            maximum_pico_usd: PicoUsd::new(
                                (index as u128 + 1) * 12_500_000_000_000,
                            ),
                            observed_samples: 100 * (index as u64 + 1),
                            priced_samples: 100 * (index as u64 + 1),
                            observed_tokens: token_usage.total_tokens,
                            priced_tokens: token_usage.total_tokens,
                        }
                    },
                },
            },
        )
        .collect::<Vec<_>>();
    let mut total = TokenUsage::default();
    for usage in usages {
        total.add_assign(usage);
    }
    let analysis = &mut harness.app.snapshot.window_analyses[0];
    analysis.threads = threads;
    analysis.attribution.local_token_usage = total;
    analysis.attribution.proxy_projected_percent = 44.0;
    analysis.attribution.unattributed_percent = 44.0;
    let window = analysis.attribution.window.as_mut().unwrap();
    window.used_percent = 44.0;
    window.starts_at = DateTime::parse_from_rfc3339("2026-10-02T21:13:36Z")
        .unwrap()
        .with_timezone(&Utc);
    window.ends_at = window.starts_at + ChronoDuration::weeks(1);
    harness.app.window_scope = WindowScope::Week;
    harness.app.local_snapshot.tasks.clear();
    harness.app.task_list_mode = TaskListMode::Tree;
    harness.app.selected_task = 0;
    harness.render();
    harness
}

fn align_usage_at_top(harness: &mut TuiHarness) {
    harness.key(KeyCode::Home);
    let limit = harness.app.entity_detail.as_ref().unwrap().line_count;
    for _ in 0..=limit {
        let content = harness.app.entity_detail_hitbox.unwrap().content;
        if rendered_row(harness, content, content.y).trim() == "Usage" {
            return;
        }
        let before = harness.app.entity_detail.as_ref().unwrap().offset;
        harness.key(KeyCode::Down);
        if harness.app.entity_detail.as_ref().unwrap().offset == before {
            break;
        }
    }
    panic!("the Usage heading must be reachable at the viewport top");
}

fn entry(harness: &TuiHarness, focus: Focus) -> Rect {
    match focus {
        Focus::Tasks => harness.app.task_controls_hitbox.unwrap().details,
        Focus::Turns => harness.app.turn_controls_hitbox.unwrap().details,
        _ => panic!("expected a table focus"),
    }
}

fn click_at(harness: &mut TuiHarness, area: Rect, column: u16) -> bool {
    let handled = handle_mouse_event(
        &mut harness.app,
        mouse_event(MouseEventKind::Down(MouseButton::Left), column, area.y),
    );
    harness.render();
    handled
}

fn mouse_at(harness: &mut TuiHarness, kind: MouseEventKind, column: u16, row: u16) -> bool {
    let handled = handle_mouse_event(&mut harness.app, mouse_event(kind, column, row));
    harness.render();
    handled
}

fn assert_binding(harness: &TuiHarness, area: Rect, key: &str, active: bool) {
    assert!(!area.is_empty(), "{key} control must be visible");
    let binding = key
        .chars()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    let mut bindings = 0;
    for column in area.x..area.right() {
        let (symbol, foreground, modifier) = harness.cell_style(column, area.y);
        let index = column.saturating_sub(area.x.saturating_add(1));
        if column > area.x && usize::from(index) < binding.len() {
            bindings += 1;
            assert_eq!(symbol, binding[usize::from(index)]);
            assert_eq!(foreground == harness.app.theme.palette().accent, active);
            assert_eq!(modifier.contains(Modifier::BOLD), active);
        } else {
            assert_ne!(
                foreground,
                harness.app.theme.palette().accent,
                "accents {symbol:?}"
            );
        }
    }
    assert_eq!(
        bindings,
        binding.len(),
        "all exact shortcut graphemes in {area:?}"
    );
}

fn open(harness: &mut TuiHarness) {
    assert!(!harness.key(KeyCode::F(2)));
    assert!(harness.app.entity_detail.is_some());
}

fn open_with_lines(harness: &mut TuiHarness, lines: Vec<Line<'static>>) {
    assert!(!handle_key_event(
        &mut harness.app,
        key_event(KeyCode::F(2))
    ));
    let theme = harness.app.theme;
    let popup = harness
        .app
        .entity_detail
        .as_mut()
        .expect("details must open");
    popup.document = vec![DetailNode::Lines(lines)];
    // These plain lines have no width-dependent Usage blocks; render wraps them.
    popup.rebuild(1, theme);
    harness.render();
}

fn section_ids(document: &[DetailNode], foldable_only: bool) -> Vec<String> {
    let mut ids = Vec::new();
    for node in document {
        if let DetailNode::Section {
            id,
            foldable,
            children,
            ..
        } = node
        {
            if !foldable_only || *foldable {
                ids.push(id.clone());
            }
            ids.extend(section_ids(children, foldable_only));
        }
    }
    ids
}

fn has_usage_node(document: &[DetailNode]) -> bool {
    document.iter().any(|node| match node {
        DetailNode::Usage(_) => true,
        DetailNode::Section { children, .. } => has_usage_node(children),
        _ => false,
    })
}

fn section_children<'a>(document: &'a [DetailNode], target: &str) -> Option<&'a [DetailNode]> {
    for node in document {
        if let DetailNode::Section { id, children, .. } = node {
            if id == target {
                return Some(children);
            }
            if let Some(found) = section_children(children, target) {
                return Some(found);
            }
        }
    }
    None
}

fn expand_all(harness: &mut TuiHarness) {
    loop {
        let popup = harness.app.entity_detail.as_ref().unwrap();
        let unopened = section_ids(&popup.document, true)
            .into_iter()
            .filter(|id| !popup.expanded.contains(id))
            .collect::<Vec<_>>();
        if unopened.is_empty() {
            break;
        }
        let width = harness
            .app
            .entity_detail_hitbox
            .map_or(1, |hitbox| hitbox.content.width);
        let theme = harness.app.theme;
        let popup = harness.app.entity_detail.as_mut().unwrap();
        popup.expanded.extend(unopened);
        popup.rebuild(width, theme);
        harness.render();
    }
}

fn open_expanded(harness: &mut TuiHarness) {
    open(harness);
    expand_all(harness);
}

fn section_expanded(harness: &TuiHarness, id: &str) -> bool {
    harness
        .app
        .entity_detail
        .as_ref()
        .unwrap()
        .expanded
        .contains(id)
}

fn closed_preview_teaser(harness: &TuiHarness) -> String {
    let popup = harness.app.entity_detail.as_ref().unwrap();
    assert!(!popup.expanded.contains(MESSAGE_PREVIEW_SECTION));
    let header = popup
        .headers
        .iter()
        .find(|header| header.id == MESSAGE_PREVIEW_SECTION)
        .expect("the collapsed preview has its own heading");
    let teaser = popup.lines[header.line + 1].to_string();
    assert!(teaser.trim_start().starts_with("Saved preview:"));
    assert_eq!(
        popup
            .lines
            .iter()
            .filter(|line| line.to_string().trim_start().starts_with("Saved preview:"))
            .count(),
        1,
        "a collapsed preview contributes exactly one teaser line"
    );
    assert!(
        UnicodeWidthStr::width(teaser.as_str())
            <= usize::from(harness.app.entity_detail_hitbox.unwrap().content.width),
        "the collapsed teaser must fit one actual content row: {teaser:?}"
    );
    assert!(!teaser.chars().any(char::is_control));
    assert!(!teaser.contains('\u{202e}'));
    teaser
}

fn focus_section(harness: &mut TuiHarness, id: &str) -> Rect {
    let limit = harness.app.entity_detail.as_ref().unwrap().headers.len();
    for _ in 0..=limit {
        if harness
            .app
            .entity_detail
            .as_ref()
            .unwrap()
            .selected_section
            .as_deref()
            == Some(id)
        {
            // Cycling also scrolls an offscreen selected header into view.
            if let Some((_, rect)) = harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .section_hitboxes
                .iter()
                .find(|(section, _)| section == id)
            {
                return *rect;
            }
        }
        let popup = harness.app.entity_detail.as_ref().unwrap();
        let target = popup
            .headers
            .iter()
            .position(|header| header.id == id)
            .unwrap_or_else(|| panic!("section {id} must be visible through its ancestors"));
        let current = popup
            .headers
            .iter()
            .position(|header| Some(&header.id) == popup.selected_section.as_ref());
        let backwards = match current {
            Some(current) => {
                (current + limit - target) % limit < (target + limit - current) % limit
            }
            None => target >= limit / 2,
        };
        harness.key(if backwards {
            KeyCode::BackTab
        } else {
            KeyCode::Tab
        });
    }
    panic!("section {id} must be selectable and reachable");
}

fn focus_preview_with_teaser(harness: &mut TuiHarness) -> Rect {
    let mut heading = focus_section(harness, MESSAGE_PREVIEW_SECTION);
    if heading.bottom() >= harness.app.entity_detail_hitbox.unwrap().body.bottom() {
        harness.key(KeyCode::Down);
        heading = focus_section(harness, MESSAGE_PREVIEW_SECTION);
    }
    assert!(heading.bottom() < harness.app.entity_detail_hitbox.unwrap().body.bottom());
    heading
}

fn first_tool_call_id(harness: &TuiHarness) -> String {
    let document = &harness.app.entity_detail.as_ref().unwrap().document;
    let calls = section_children(document, "recorded.tools").expect("tool section");
    calls
        .iter()
        .find_map(|node| match node {
            DetailNode::Section { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("one independently foldable call")
}

fn tool_body_ids(harness: &TuiHarness, call: &str) -> (String, String) {
    let document = &harness.app.entity_detail.as_ref().unwrap().document;
    let children = section_children(document, call).expect("expanded call section");
    let child = |label: &str| {
        children
            .iter()
            .find_map(|node| match node {
                DetailNode::Section { id, title, .. } if title.contains(label) => Some(id.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{label} section inside call"))
    };
    (child("Arguments"), child("Output"))
}

fn tool_layer_ids(harness: &TuiHarness) -> (String, String, String) {
    let call = first_tool_call_id(harness);
    let (arguments, output) = tool_body_ids(harness, &call);
    (call, arguments, output)
}

fn expand_tool_layers(harness: &mut TuiHarness) {
    if !section_expanded(harness, "recorded.tools") {
        assert!(harness.app.toggle_entity_detail_section("recorded.tools"));
        harness.render();
    }
    let call = first_tool_call_id(harness);
    if !section_expanded(harness, &call) {
        assert!(harness.app.toggle_entity_detail_section(&call));
        harness.render();
    }
    let (arguments, output) = tool_body_ids(harness, &call);
    for id in [arguments, output] {
        if !section_expanded(harness, &id) {
            assert!(harness.app.toggle_entity_detail_section(&id));
            harness.render();
        }
    }
}

fn save_gallery(harness: &TuiHarness, name: &str) {
    let directory = gallery_directory();
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{name}.svg")),
        harness.frame().to_svg(name),
    )
    .unwrap();
}

fn rendered_row(harness: &TuiHarness, area: Rect, row: u16) -> String {
    let mut text = String::new();
    let mut column = area.x;
    while column < area.right() {
        let symbol = harness.cell_style(column, row).0;
        // A wide grapheme's continuation cell is not another space in its text.
        column += UnicodeWidthStr::width(symbol.as_str()).max(1) as u16;
        text.push_str(&symbol);
    }
    text
}

fn rendered_region(harness: &TuiHarness, area: Rect) -> String {
    (area.y..area.bottom())
        .map(|row| rendered_row(harness, area, row))
        .collect::<Vec<_>>()
        .join("\n")
}

fn scroll_to_body_marker(harness: &mut TuiHarness, marker: &str) {
    let limit = harness.app.entity_detail.as_ref().unwrap().line_count;
    for _ in 0..=limit {
        let body = harness.app.entity_detail_hitbox.unwrap().body;
        if rendered_row(harness, body, body.y).contains(marker) {
            return;
        }
        let previous = harness.app.entity_detail.as_ref().unwrap().offset;
        harness.key(KeyCode::Down);
        assert_ne!(
            harness.app.entity_detail.as_ref().unwrap().offset,
            previous,
            "{marker} must remain reachable in the actual body viewport"
        );
    }
    panic!("{marker} must appear within the finite document");
}

fn sticky_headers(harness: &TuiHarness) -> Vec<(String, Rect)> {
    let controls = harness.app.entity_detail_hitbox.unwrap();
    let mut headers = harness
        .app
        .entity_detail
        .as_ref()
        .unwrap()
        .section_hitboxes
        .iter()
        .filter(|(_, rect)| rect.y < controls.body.y)
        .cloned()
        .collect::<Vec<_>>();
    headers.sort_by_key(|(_, rect)| rect.y);
    headers
}

fn assert_section_binding(harness: &TuiHarness, rect: Rect, active: bool) {
    let mut found = 0;
    for row in rect.y..rect.bottom() {
        for column in rect.x..rect.right() {
            let (symbol, foreground, modifier) = harness.cell_style(column, row);
            if symbol == "↵" {
                found += 1;
                assert_eq!(foreground == harness.app.theme.palette().accent, active);
                assert_eq!(modifier.contains(Modifier::BOLD), active);
            } else {
                assert_ne!(foreground, harness.app.theme.palette().accent);
            }
        }
    }
    assert_eq!(found, 1, "the exact Enter binding appears once in {rect:?}");
}

fn sticky_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness = detail_harness(width, height, theme);
    harness.app.local_snapshot.tasks.clear();
    open_with_lines(&mut harness, Vec::new());
    let popup = harness.app.entity_detail.as_mut().unwrap();
    popup.document = vec![
        DetailNode::Section {
            id: "fixture.tools".to_string(),
            title: "Tool calls (812)".to_string(),
            foldable: true,
            children: vec![
                DetailNode::Section {
                    id: "fixture.call".to_string(),
                    title: format!("exec_command | {} CALL-TITLE-END", "调用 👩‍💻 ".repeat(24)),
                    foldable: true,
                    children: vec![DetailNode::Section {
                        id: "fixture.output".to_string(),
                        title: format!("Output | {} OUTPUT-TITLE-END", "工具输出 🧑‍💻 ".repeat(24)),
                        foldable: true,
                        children: vec![DetailNode::Lines(
                            (0..100)
                                .map(|index| Line::from(format!("OUTPUT-{index:03}")))
                                .collect(),
                        )],
                    }],
                },
                DetailNode::Section {
                    id: "fixture.sibling".to_string(),
                    title: "Next tool".to_string(),
                    foldable: true,
                    children: vec![DetailNode::Lines(
                        (0..60)
                            .map(|index| Line::from(format!("SIBLING-{index:03}")))
                            .collect(),
                    )],
                },
            ],
        },
        DetailNode::Section {
            id: "fixture.following".to_string(),
            title: "Related".to_string(),
            foldable: true,
            children: vec![DetailNode::Lines(
                (0..60)
                    .map(|index| Line::from(format!("TAIL-{index:03}")))
                    .collect(),
            )],
        },
    ];
    popup.expanded.extend(
        [
            "fixture.tools",
            "fixture.call",
            "fixture.output",
            "fixture.sibling",
            "fixture.following",
        ]
        .map(str::to_string),
    );
    popup.rebuild(1, theme);
    harness.render();
    harness
}

#[test]
fn entity_detail_default_width_uses_the_terminal_up_to_140_columns() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in [(32, 14), (60, 24), (120, 40), (160, 40), (200, 40)] {
            let mut harness = recorded_tools_harness(width, height, theme, true);
            let controls = harness.app.entity_detail_hitbox.unwrap();
            let popup_width = width.saturating_sub(4).min(140);
            assert_eq!(controls.content.width, popup_width - 3);
            assert_eq!(controls.content.x, (width - popup_width) / 2 + 1);
            assert!(controls.content.right() < width);
            assert_eq!(
                controls.body, controls.content,
                "folded groups have no pins"
            );
            assert_eq!(
                controls.scrollbar.unwrap().track.x,
                controls.content.right()
            );
            if width == 160 && theme == Theme::Dark {
                save_gallery(&harness, "entity-detail-wide-dark-160x40");
            }
            harness.resize(32, 14);
            assert_eq!(harness.app.entity_detail_hitbox.unwrap().content.width, 25);
            harness.resize(width, height);
            assert_eq!(
                harness.app.entity_detail_hitbox.unwrap().content,
                controls.content
            );
            if width == 160 && theme == Theme::Dark {
                expand_tool_layers(&mut harness);
                let (call, _, output) = tool_layer_ids(&harness);
                focus_section(&mut harness, &output);
                scroll_to_body_marker(&mut harness, "recorded tool output row 030");
                assert_eq!(
                    sticky_headers(&harness)
                        .iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>(),
                    ["recorded.tools", call.as_str(), output.as_str()]
                );
                save_gallery(&harness, "entity-detail-sticky-tools-dark-160x40");
            }
        }
    }
}

#[test]
fn entity_detail_sticky_headers_follow_expanded_ancestors_and_leave_body_space() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in [(120, 40), (32, 14)] {
            let mut harness = sticky_harness(width, height, theme);
            assert!(sticky_headers(&harness).is_empty());
            focus_section(&mut harness, "fixture.output");
            scroll_to_body_marker(&mut harness, "OUTPUT-030");
            let controls = harness.app.entity_detail_hitbox.unwrap();
            let pinned = sticky_headers(&harness);
            assert_eq!(
                pinned.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                ["fixture.tools", "fixture.call", "fixture.output"]
            );
            assert_eq!(controls.body.y, controls.content.y + 3);
            assert_eq!(controls.body.height, controls.content.height - 3);
            assert!(controls.body.height >= 2);
            assert_eq!(controls.body.bottom(), controls.content.bottom());
            for (index, (id, rect)) in pinned.iter().enumerate() {
                assert_eq!(rect.y, controls.content.y + index as u16);
                assert_eq!(rect.height, 1);
                assert!(rect.right() <= controls.content.right());
                assert_section_binding(&harness, *rect, id == "fixture.output");
                let title = rendered_row(&harness, *rect, rect.y);
                assert!(UnicodeWidthStr::width(title.as_str()) <= usize::from(rect.width));
                if id != "fixture.tools" {
                    assert!(title.contains('…'), "long pinned Unicode title: {title:?}");
                }
            }
            assert!(popup_text(&harness.app).contains("CALL-TITLE-END"));
            assert!(popup_text(&harness.app).contains("OUTPUT-TITLE-END"));
            assert!(rendered_region(&harness, controls.body).contains("OUTPUT-030"));
            let body_offset = harness.app.entity_detail.as_ref().unwrap().offset;
            let tools = focus_section(&mut harness, "fixture.tools");
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                body_offset
            );
            assert_section_binding(&harness, tools, true);
            focus_section(&mut harness, "fixture.output");
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                body_offset
            );
            for key in ['t', 'T', ' '] {
                let before = harness.app.entity_detail.as_ref().unwrap().expanded.clone();
                harness.key(KeyCode::Char(key));
                assert_eq!(harness.app.entity_detail.as_ref().unwrap().expanded, before);
            }
            let offset = harness.app.entity_detail.as_ref().unwrap().offset;
            harness.key(KeyCode::PageDown);
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                offset + usize::from(controls.body.height)
            );
            harness.key(KeyCode::PageUp);
            assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, offset);
            if width == 120 && theme == Theme::Dark {
                harness.resize(160, 40);
                scroll_to_body_marker(&mut harness, "OUTPUT-030");
                save_gallery(&harness, "entity-detail-sticky-dark-160x40");
                harness.resize(width, height);
            }
            if width == 32 && theme == Theme::Light {
                save_gallery(&harness, "entity-detail-sticky-light-32x14");
            }
            scroll_to_body_marker(&mut harness, "SIBLING-030");
            assert_eq!(
                sticky_headers(&harness)
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                ["fixture.tools", "fixture.sibling"]
            );
            scroll_to_body_marker(&mut harness, "TAIL-020");
            assert_eq!(
                sticky_headers(&harness)
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                ["fixture.following"]
            );
            harness.key(KeyCode::End);
            let end = harness.app.entity_detail_hitbox.unwrap();
            let popup = harness.app.entity_detail.as_ref().unwrap();
            assert_eq!(
                popup.offset,
                popup.scroll_limit(usize::from(end.content.height))
            );
            assert_eq!(popup.offset, end.scrollbar.unwrap().max_offset);
            assert!(rendered_region(&harness, end.body).contains("TAIL-059"));
            assert_binding(&harness, end.down, "↓", false);
            let previous = popup.offset;
            mouse_at(
                &mut harness,
                MouseEventKind::ScrollDown,
                end.body.x,
                end.body.y,
            );
            assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, previous);
            harness.key(KeyCode::Home);
            let bar = harness.app.entity_detail_hitbox.unwrap().scrollbar.unwrap();
            mouse_at(
                &mut harness,
                MouseEventKind::Down(MouseButton::Left),
                bar.thumb.x,
                bar.thumb.y,
            );
            mouse_at(
                &mut harness,
                MouseEventKind::Drag(MouseButton::Left),
                bar.track.x,
                bar.track.bottom() - 1,
            );
            mouse_at(
                &mut harness,
                MouseEventKind::Up(MouseButton::Left),
                bar.track.x,
                bar.track.bottom() - 1,
            );
            let dragged = harness.app.entity_detail_hitbox.unwrap();
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                dragged.scrollbar.unwrap().max_offset
            );
            assert!(rendered_region(&harness, dragged.body).contains("TAIL-059"));
            assert!(harness.app.scroll_drag.is_none());
        }
    }
}

#[test]
fn entity_detail_sticky_whole_labels_collapse_their_sections_and_restore_focus() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in [(120, 40), (32, 14)] {
            let mut harness = sticky_harness(width, height, theme);
            focus_section(&mut harness, "fixture.output");
            scroll_to_body_marker(&mut harness, "OUTPUT-030");
            for id in ["fixture.tools", "fixture.call", "fixture.output"] {
                let rect = sticky_headers(&harness)
                    .into_iter()
                    .find(|(candidate, _)| candidate == id)
                    .unwrap()
                    .1;
                let columns = if width == 32 {
                    (rect.x..rect.right()).collect::<Vec<_>>()
                } else {
                    vec![rect.x, rect.right() - 1]
                };
                for column in columns {
                    let current = sticky_headers(&harness)
                        .into_iter()
                        .find(|(candidate, _)| candidate == id)
                        .unwrap()
                        .1;
                    assert!(click_at(&mut harness, current, column));
                    assert!(!section_expanded(&harness, id));
                    let popup = harness.app.entity_detail.as_ref().unwrap();
                    assert_eq!(popup.selected_section.as_deref(), Some(id));
                    assert!(!popup_text(&harness.app).contains("OUTPUT-030"));
                    let heading = popup
                        .section_hitboxes
                        .iter()
                        .find(|(candidate, _)| candidate == id)
                        .unwrap()
                        .1;
                    let body = harness.app.entity_detail_hitbox.unwrap().body;
                    assert!(
                        heading.y >= body.y,
                        "collapse restores the real flow heading"
                    );
                    assert_section_binding(&harness, heading, true);
                    harness.key(KeyCode::Enter);
                    assert!(section_expanded(&harness, id));
                    scroll_to_body_marker(&mut harness, "OUTPUT-030");
                }
            }
            focus_section(&mut harness, "fixture.output");
            scroll_to_body_marker(&mut harness, "OUTPUT-030");
            focus_section(&mut harness, "fixture.tools");
            for (new_width, new_height) in [(32, 14), (120, 40), (width, height)] {
                harness.resize(new_width, new_height);
                assert_eq!(
                    harness
                        .app
                        .entity_detail
                        .as_ref()
                        .unwrap()
                        .selected_section
                        .as_deref(),
                    Some("fixture.tools")
                );
                assert!(
                    harness
                        .app
                        .entity_detail
                        .as_ref()
                        .unwrap()
                        .section_hitboxes
                        .iter()
                        .any(|(id, rect)| id == "fixture.tools" && !rect.is_empty())
                );
                assert!(harness.app.entity_detail_hitbox.unwrap().body.height >= 2);
                let body = harness.app.entity_detail_hitbox.unwrap().body;
                assert!(
                    rendered_row(&harness, body, body.y).contains("OUTPUT-030"),
                    "resize retains the body position beneath pinned ancestors"
                );
            }
            for (new_width, new_height) in [(32, 11), (24, 10), (16, 9), (8, 5), (3, 2), (1, 1)] {
                harness.resize(120, 40);
                focus_section(&mut harness, "fixture.output");
                scroll_to_body_marker(&mut harness, "OUTPUT-030");
                harness.resize(new_width, new_height);
                let controls = harness.app.entity_detail_hitbox.unwrap();
                let pins = sticky_headers(&harness);
                assert!(pins.len() <= 3);
                assert!(pins.len() <= usize::from(controls.content.height.saturating_sub(2)));
                if new_height >= 10 {
                    let expected = ["fixture.tools", "fixture.call", "fixture.output"];
                    let count = usize::from(controls.content.height.saturating_sub(2)).min(3);
                    assert_eq!(
                        pins.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
                        expected[3 - count..]
                    );
                }
                if controls.content.height >= 2 {
                    assert!(controls.body.height >= 2);
                }
                harness.key(KeyCode::End);
                assert!(harness.app.entity_detail.is_some());
            }
        }
    }
}

#[test]
fn entity_detail_sticky_tail_limit_does_not_oscillate_when_an_ancestor_ends() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut harness = detail_harness(60, 17, theme);
        harness.app.local_snapshot.tasks.clear();
        open_with_lines(&mut harness, Vec::new());
        let popup = harness.app.entity_detail.as_mut().unwrap();
        popup.document = vec![
            DetailNode::Section {
                id: "fixture.ending".to_string(),
                title: "Ending ancestor".to_string(),
                foldable: true,
                children: vec![DetailNode::Lines(
                    (0..90)
                        .map(|index| Line::from(format!("ANCESTOR-{index:03}")))
                        .collect(),
                )],
            },
            DetailNode::Lines(
                (0..9)
                    .map(|index| Line::from(format!("TAIL-{index:03}")))
                    .collect(),
            ),
        ];
        popup.expanded.insert("fixture.ending".to_string());
        popup.rebuild(1, theme);
        harness.render();
        let content = harness.app.entity_detail_hitbox.unwrap().content;
        assert_eq!(content.height, 10);
        assert_eq!(harness.app.entity_detail.as_ref().unwrap().line_count, 100);
        assert_eq!(
            harness.app.entity_detail.as_ref().unwrap().scroll_limit(10),
            91
        );
        harness.app.entity_detail.as_mut().unwrap().offset = 90;
        harness.render();
        assert_eq!(sticky_headers(&harness).len(), 1);
        assert_eq!(harness.app.entity_detail_hitbox.unwrap().body.height, 9);
        assert!(
            !rendered_region(&harness, harness.app.entity_detail_hitbox.unwrap().body)
                .contains("TAIL-008")
        );
        harness.key(KeyCode::Down);
        assert!(sticky_headers(&harness).is_empty());
        assert_eq!(harness.app.entity_detail_hitbox.unwrap().body.height, 10);
        for _ in 0..4 {
            harness.render();
            harness.key(KeyCode::Down);
            assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 91);
            assert_eq!(
                harness
                    .app
                    .entity_detail_hitbox
                    .unwrap()
                    .scrollbar
                    .unwrap()
                    .max_offset,
                91
            );
            assert!(
                rendered_region(&harness, harness.app.entity_detail_hitbox.unwrap().body)
                    .contains("TAIL-008")
            );
        }
        harness.key(KeyCode::Home);
        harness.key(KeyCode::End);
        assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 91);
    }
}

#[test]
fn entity_detail_sticky_scrollbar_drag_reaches_the_end_after_the_thumb_shrinks() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut harness = detail_harness(80, 42, theme);
        harness.app.local_snapshot.tasks.clear();
        open_with_lines(&mut harness, Vec::new());
        let popup = harness.app.entity_detail.as_mut().unwrap();
        popup.document = vec![DetailNode::Section {
            id: "fixture.tools".to_string(),
            title: "Tool calls".to_string(),
            foldable: true,
            children: vec![DetailNode::Section {
                id: "fixture.call".to_string(),
                title: "exec_command".to_string(),
                foldable: true,
                children: vec![DetailNode::Section {
                    id: "fixture.output".to_string(),
                    title: "Output".to_string(),
                    foldable: true,
                    children: vec![DetailNode::Lines(
                        (0..80)
                            .map(|index| Line::from(format!("OUTPUT-{index:03}")))
                            .collect(),
                    )],
                }],
            }],
        }];
        popup
            .expanded
            .extend(["fixture.tools", "fixture.call", "fixture.output"].map(str::to_string));
        popup.rebuild(1, theme);
        harness.render();
        let initial = harness.app.entity_detail_hitbox.unwrap();
        assert_eq!(initial.content.height, 35);
        assert_eq!(initial.body.height, 35);
        assert_eq!(harness.app.entity_detail.as_ref().unwrap().line_count, 83);
        let bar = initial.scrollbar.unwrap();
        assert_eq!(bar.max_offset, 51);
        assert_eq!(bar.thumb.height, 15);
        mouse_at(
            &mut harness,
            MouseEventKind::Down(MouseButton::Left),
            bar.thumb.x,
            bar.thumb.bottom() - 1,
        );
        assert_eq!(harness.app.scroll_drag.unwrap().grab_row, 14);
        mouse_at(
            &mut harness,
            MouseEventKind::Drag(MouseButton::Left),
            bar.track.x,
            bar.track.y + 25,
        );
        let shrunk = harness.app.entity_detail_hitbox.unwrap();
        assert_eq!(sticky_headers(&harness).len(), 3);
        assert_eq!(shrunk.body.height, 32);
        assert_eq!(shrunk.scrollbar.unwrap().thumb.height, 14);
        mouse_at(
            &mut harness,
            MouseEventKind::Drag(MouseButton::Left),
            bar.track.x,
            bar.track.bottom() - 1,
        );
        let end = harness.app.entity_detail_hitbox.unwrap();
        assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 51);
        assert_eq!(
            harness.app.entity_detail.as_ref().unwrap().offset,
            end.scrollbar.unwrap().max_offset
        );
        assert_eq!(harness.app.scroll_drag.unwrap().grab_row, 13);
        assert!(rendered_region(&harness, end.body).contains("OUTPUT-079"));
        mouse_at(
            &mut harness,
            MouseEventKind::Up(MouseButton::Left),
            bar.track.x,
            bar.track.bottom() - 1,
        );
        assert!(harness.app.scroll_drag.is_none());
    }
}

fn recorded_tools_fixture(
    captured: DateTime<Utc>,
    with_tools: bool,
) -> crate::session_details::SessionDetails {
    use crate::session_details::{DetailMessage, DetailTool, DetailUsage, SessionDetails};

    let mut data = SessionDetails::default();
    data.files_read = 1;
    data.messages.push(DetailMessage {
        role: "user".to_string(),
        text: "MESSAGE-BODY-STAYS-HIDDEN-WHEN-TOOLS-EXPAND".to_string(),
        timestamp: Some(captured),
        turn_id: Some("fixture-turn".to_string()),
        phase: Some("commentary".to_string()),
    });
    data.metadata
        .insert("Approval policy".to_string(), "on-request".to_string());
    if with_tools {
        data.tools.push(DetailTool {
            call_id: "fixture-tool-call".to_string(),
            name: "exec_command".to_string(),
            arguments: Some("TOOL-ARGUMENTS-MARKER cargo test fixture".to_string()),
            output: Some(format!(
                "{}\n工具输出 🧑‍💻 TOOL-OUTPUT-END",
                (0..80)
                    .map(|index| format!("recorded tool output row {index:03}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
            exit_code: Some(0),
            duration_ms: Some(125),
            timestamp: Some(captured),
            turn_id: Some("fixture-turn".to_string()),
            test_command: true,
        });
    }
    data.usage.push(DetailUsage {
        timestamp: Some(captured),
        turn_id: Some("fixture-turn".to_string()),
        model: Some("gpt-5.6-sol".to_string()),
        service_tier: Some("default".to_string()),
        tokens: TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            ..TokenUsage::default()
        },
        exact: true,
    });
    data
}

fn recorded_tools_harness(width: u16, height: u16, theme: Theme, with_tools: bool) -> TuiHarness {
    let mut harness = detail_harness(width, height, theme);
    // This fixture uses injected evidence and never starts a rollout reader.
    harness.app.local_snapshot.tasks.clear();
    open(&mut harness);
    let data = recorded_tools_fixture(harness.app.snapshot.as_of, with_tools);
    harness
        .app
        .entity_detail
        .as_mut()
        .unwrap()
        .set_recorded_details(data, theme);
    harness.render();
    harness
}

fn reveal_tools_header(harness: &mut TuiHarness) -> Rect {
    focus_section(harness, "recorded.tools")
}

#[test]
fn entity_detail_large_tool_list_projects_only_expanded_arguments_and_output() {
    use crate::session_details::{DetailEvidence, DetailTool};

    for (width, height, theme) in [(120, 40, Theme::Dark), (32, 14, Theme::Light)] {
        let mut harness = recorded_tools_harness(width, height, theme, false);
        let captured = harness.app.snapshot.as_of;
        let mut data = recorded_tools_fixture(captured, false);
        data.tools = (0..812)
            .map(|index| DetailTool {
                call_id: if index == 811 {
                    format!("call-{index:04}-{}ID-END", "调用👩‍💻".repeat(32))
                } else {
                    format!("call-{index:04}")
                },
                name: format!("tool_{index:03}"),
                arguments: Some(format!("ARGS-{index:03}")),
                output: Some(if index == 400 {
                    format!("UNOPENED-OUTPUT\n{}", "x".repeat(60 * 1024))
                } else {
                    format!("OUT-{index:03}")
                }),
                exit_code: Some(0),
                duration_ms: Some(1),
                timestamp: Some(captured),
                turn_id: Some("fixture-turn".to_string()),
                test_command: false,
            })
            .collect();
        data.file_changes = (0..16)
            .map(|index| DetailEvidence {
                text: format!("UNOPENED-DIFF-{index}\n{}", "+x".repeat(30 * 1024)),
                timestamp: Some(captured),
                turn_id: Some("fixture-turn".to_string()),
            })
            .collect();
        harness
            .app
            .entity_detail
            .as_mut()
            .unwrap()
            .set_recorded_details(data, theme);
        harness.render();
        focus_section(&mut harness, "recorded.tools");
        harness.key(KeyCode::Enter);
        let calls = section_children(
            &harness.app.entity_detail.as_ref().unwrap().document,
            "recorded.tools",
        )
        .unwrap()
        .iter()
        .filter_map(|node| match node {
            DetailNode::Section { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
        assert_eq!(calls.len(), 812, "every recorded call remains selectable");
        assert!(!popup_text(&harness.app).contains("ARGS-811"));
        assert!(!popup_text(&harness.app).contains("OUT-811"));

        for index in [811, 0] {
            let call = &calls[index];
            focus_section(&mut harness, call);
            harness.key(KeyCode::Enter);
            let (arguments, output) = tool_body_ids(&harness, call);
            for (leaf, marker) in [
                (&arguments, format!("ARGS-{index:03}")),
                (&output, format!("OUT-{index:03}")),
            ] {
                focus_section(&mut harness, leaf);
                harness.key(KeyCode::Enter);
                let header = focus_section(&mut harness, leaf);
                let body = harness.app.entity_detail_hitbox.unwrap().body;
                for _ in 0..header.y.saturating_sub(body.y) {
                    harness.key(KeyCode::Down);
                }
                assert!(
                    popup_text(&harness.app).contains(&marker),
                    "expanded {leaf} retains its recorded body at {width}x{height}"
                );
                assert!(
                    harness.frame().snapshot_text().contains(&marker),
                    "expanded {leaf} displays {marker} in the viewport"
                );
                for unopened in [
                    "UNOPENED-DIFF",
                    "UNOPENED-OUTPUT",
                    "MESSAGE-BODY-STAYS-HIDDEN",
                ] {
                    assert!(!popup_text(&harness.app).contains(unopened));
                }
                focus_section(&mut harness, leaf);
                harness.key(KeyCode::Enter);
                assert!(!section_expanded(&harness, leaf));
            }
            if index == 811 && width == 32 {
                focus_section(&mut harness, call);
                // Selecting a pinned ancestor preserves the body position.
                // Scroll back to its real flow heading before inspecting the
                // metadata between that heading and Arguments / Output.
                let header = loop {
                    let body = harness.app.entity_detail_hitbox.unwrap().body;
                    if let Some((_, rect)) = harness
                        .app
                        .entity_detail
                        .as_ref()
                        .unwrap()
                        .section_hitboxes
                        .iter()
                        .find(|(id, rect)| id == call && rect.y >= body.y)
                    {
                        break *rect;
                    }
                    let previous = harness.app.entity_detail.as_ref().unwrap().offset;
                    harness.key(KeyCode::Up);
                    assert_ne!(
                        harness.app.entity_detail.as_ref().unwrap().offset,
                        previous,
                        "the real call heading must be reachable above its pinned copy"
                    );
                };
                let content = harness.app.entity_detail_hitbox.unwrap().content;
                let body = harness.app.entity_detail_hitbox.unwrap().body;
                for _ in 0..header.y.saturating_sub(body.y) + header.height {
                    harness.key(KeyCode::Down);
                }
                let mut found_continuation = false;
                let mut found_tail = false;
                for _ in 0..40 {
                    for row in content.y..content.bottom() {
                        let text = (content.x..content.right())
                            .map(|column| harness.cell_style(column, row).0)
                            .collect::<String>();
                        // Wide graphemes occupy a symbol cell plus a blank
                        // continuation cell, so raw cells do not join as 调用.
                        if !text.contains("Call ID:")
                            && (text.contains('调') || text.contains('用'))
                        {
                            found_continuation = true;
                            assert!(text.starts_with("    "), "nested continuation: {text:?}");
                        }
                        if text.contains("ID-END") {
                            found_tail = true;
                            assert!(text.starts_with("    "), "nested ID tail: {text:?}");
                        }
                    }
                    if found_tail {
                        break;
                    }
                    harness.key(KeyCode::Down);
                }
                assert!(
                    found_continuation,
                    "long Unicode call ID wraps into the viewport"
                );
                assert!(
                    found_tail,
                    "the full call ID remains reachable after wrapping"
                );
                focus_section(&mut harness, &output);
                harness.key(KeyCode::Enter);
                let header = focus_section(&mut harness, &output);
                let body = harness.app.entity_detail_hitbox.unwrap().body;
                for _ in 0..header.y.saturating_sub(body.y) {
                    harness.key(KeyCode::Down);
                }
                save_gallery(&harness, "entity-tools-large-list-light-32x14");
                focus_section(&mut harness, &output);
                harness.key(KeyCode::Enter);
            }
            focus_section(&mut harness, call);
            harness.key(KeyCode::Enter);
            assert!(!section_expanded(&harness, call));
        }
        assert!(!section_expanded(&harness, "recorded.files"));
    }
}

#[test]
fn entity_detail_tools_use_enter_for_the_list_call_arguments_and_output() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = recorded_tools_harness(width, height, theme, true);
            let initial = harness.state();
            assert!(!section_expanded(&harness, "recorded.tools"));
            let collapsed = popup_text(&harness.app);
            assert!(collapsed.contains("Tool calls (1)"));
            assert!(collapsed.contains("Usage observations (1)"));
            for hidden in [
                "TOOL-ARGUMENTS-MARKER",
                "TOOL-OUTPUT-END",
                "MESSAGE-BODY-STAYS-HIDDEN",
            ] {
                assert!(!collapsed.contains(hidden));
            }
            let header = focus_section(&mut harness, "recorded.tools");
            assert_binding(&harness, header, "↵", true);
            for modifiers in [
                KeyModifiers::NONE,
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
            ] {
                for key in ['t', 'T', ' '] {
                    let before = harness.app.entity_detail.as_ref().unwrap().expanded.clone();
                    handle_key_event(
                        &mut harness.app,
                        KeyEvent::new(KeyCode::Char(key), modifiers),
                    );
                    harness.render();
                    assert_eq!(harness.app.entity_detail.as_ref().unwrap().expanded, before);
                    assert_eq!(harness.app.theme, theme);
                    assert_eq!(harness.state(), initial);
                }
            }
            for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
                handle_key_event(&mut harness.app, KeyEvent::new(KeyCode::Enter, modifiers));
                harness.render();
                assert!(!section_expanded(&harness, "recorded.tools"));
            }
            for code in ['1', 'U', 'R', 'V', 'd', 'L'] {
                harness.key(KeyCode::Char(code));
                assert_eq!(harness.state(), initial);
                assert_eq!(harness.app.theme, theme);
            }
            if width == 120 && theme == Theme::Dark {
                focus_section(&mut harness, "snapshot.Related");
                save_gallery(&harness, "entity-sections-folded-dark-120x40");
                focus_section(&mut harness, "recorded.tools");
            }
            harness.key(KeyCode::Enter);
            assert!(section_expanded(&harness, "recorded.tools"));
            let call = first_tool_call_id(&harness);
            assert!(!section_expanded(&harness, &call));
            assert!(!popup_text(&harness.app).contains("TOOL-ARGUMENTS-MARKER"));
            assert!(!popup_text(&harness.app).contains("TOOL-OUTPUT-END"));
            if width == 120 && theme == Theme::Dark {
                save_gallery(&harness, "entity-tools-list-dark-120x40");
            }
            focus_section(&mut harness, &call);
            harness.key(KeyCode::Enter);
            assert!(section_expanded(&harness, &call));
            let (arguments, output) = tool_body_ids(&harness, &call);
            assert!(!section_expanded(&harness, &arguments));
            assert!(!section_expanded(&harness, &output));
            assert!(!popup_text(&harness.app).contains("TOOL-ARGUMENTS-MARKER"));
            focus_section(&mut harness, &arguments);
            harness.key(KeyCode::Enter);
            assert!(section_expanded(&harness, &arguments));
            assert!(popup_text(&harness.app).contains("TOOL-ARGUMENTS-MARKER"));
            assert!(!popup_text(&harness.app).contains("TOOL-OUTPUT-END"));
            focus_section(&mut harness, &output);
            harness.key(KeyCode::Enter);
            assert!(section_expanded(&harness, &output));
            assert!(popup_text(&harness.app).contains("TOOL-OUTPUT-END"));
            assert!(popup_text(&harness.app).contains("工具输出 🧑‍💻"));
            assert!(!popup_text(&harness.app).contains("MESSAGE-BODY-STAYS-HIDDEN"));
            for key in ['t', 'T', ' '] {
                harness.key(KeyCode::Char(key));
                assert!(section_expanded(&harness, &output));
                assert_eq!(harness.app.theme, theme);
            }
            if width == 120 && theme == Theme::Dark {
                focus_section(&mut harness, &call);
                save_gallery(&harness, "entity-tools-nested-dark-120x40");
            }
            assert_eq!(harness.state(), initial);
            harness.key(KeyCode::Esc);
            open(&mut harness);
            let data = recorded_tools_fixture(harness.app.snapshot.as_of, true);
            harness
                .app
                .entity_detail
                .as_mut()
                .unwrap()
                .set_recorded_details(data, theme);
            harness.render();
            for id in [
                "recorded.tools",
                call.as_str(),
                arguments.as_str(),
                output.as_str(),
            ] {
                assert!(!section_expanded(&harness, id));
            }
        }
    }
}

#[test]
fn entity_detail_tools_use_the_common_footer_and_whole_wrapped_header_hitboxes() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = recorded_tools_harness(width, height, theme, true);
            let initial = harness.app.entity_detail_hitbox.unwrap();
            reveal_tools_header(&mut harness);
            assert_binding(&harness, initial.toggle, "↵", true);
            for column in initial.toggle.x..initial.toggle.right() {
                assert!(click_at(&mut harness, initial.toggle, column));
                assert!(section_expanded(&harness, "recorded.tools"));
                assert_eq!(
                    harness.app.entity_detail_hitbox.unwrap().toggle,
                    initial.toggle
                );
                assert_eq!(
                    harness.app.entity_detail_hitbox.unwrap().content,
                    initial.content
                );
                assert!(click_at(&mut harness, initial.toggle, column));
                assert!(!section_expanded(&harness, "recorded.tools"));
            }
            let header = reveal_tools_header(&mut harness);
            for row in header.y..header.bottom() {
                for column in header.x..header.right() {
                    assert!(mouse_at(
                        &mut harness,
                        MouseEventKind::Down(MouseButton::Left),
                        column,
                        row
                    ));
                    assert!(section_expanded(&harness, "recorded.tools"));
                    assert_eq!(
                        harness
                            .app
                            .entity_detail
                            .as_ref()
                            .unwrap()
                            .selected_section
                            .as_deref(),
                        Some("recorded.tools")
                    );
                    assert!(mouse_at(
                        &mut harness,
                        MouseEventKind::Down(MouseButton::Left),
                        column,
                        row
                    ));
                    assert!(!section_expanded(&harness, "recorded.tools"));
                }
            }
            harness.resize(24, 14);
            let resized = harness.app.entity_detail_hitbox.unwrap();
            assert_binding(&harness, resized.toggle, "↵", true);
            assert!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .section_hitboxes
                    .iter()
                    .any(|(id, rect)| id == "recorded.tools" && !rect.is_empty())
            );
            let wrapped = reveal_tools_header(&mut harness);
            assert!(wrapped.height > 1);
            for row in wrapped.y..wrapped.bottom() {
                for column in wrapped.x..wrapped.right() {
                    mouse_at(
                        &mut harness,
                        MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                    );
                    assert!(section_expanded(&harness, "recorded.tools"));
                    mouse_at(
                        &mut harness,
                        MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                    );
                    assert!(!section_expanded(&harness, "recorded.tools"));
                }
            }
            harness.resize(width, height);
            let restored = harness.app.entity_detail_hitbox.unwrap();
            assert_eq!(restored.next, initial.next);
            assert_eq!(restored.toggle, initial.toggle);
            assert_eq!(restored.back, initial.back);
            assert_eq!(restored.content, initial.content);
        }
    }
}

#[test]
fn entity_detail_nested_collapse_restores_focus_anchor_and_scrollbar_after_resize() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = recorded_tools_harness(width, height, theme, true);
            expand_tool_layers(&mut harness);
            let (call, _, output) = tool_layer_ids(&harness);
            focus_section(&mut harness, &output);
            for (resized_width, resized_height) in [(40, 24), (32, 14), (width, height)] {
                harness.resize(resized_width, resized_height);
                assert_eq!(
                    harness
                        .app
                        .entity_detail
                        .as_ref()
                        .unwrap()
                        .selected_section
                        .as_deref(),
                    Some(output.as_str())
                );
                assert!(
                    harness
                        .app
                        .entity_detail
                        .as_ref()
                        .unwrap()
                        .section_hitboxes
                        .iter()
                        .any(|(id, rect)| id == &output && !rect.is_empty())
                );
            }
            let expanded_count = harness.app.entity_detail.as_ref().unwrap().line_count;
            harness.key(KeyCode::End);
            let end = harness.app.entity_detail_hitbox.unwrap();
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                end.scrollbar.unwrap().max_offset
            );
            assert_binding(&harness, end.down, "↓", false);
            assert!(harness.app.toggle_entity_detail_section(&call));
            harness.render();
            let popup = harness.app.entity_detail.as_ref().unwrap();
            assert_eq!(popup.selected_section.as_deref(), Some(call.as_str()));
            assert!(!popup.headers.iter().any(|header| header.id == output));
            assert!(
                popup
                    .section_hitboxes
                    .iter()
                    .any(|(id, rect)| id == &call && !rect.is_empty())
            );
            assert!(popup.line_count < expanded_count);
            let collapsed = harness.app.entity_detail_hitbox.unwrap();
            let max_offset = popup.scroll_limit(usize::from(collapsed.content.height));
            assert!(popup.offset <= max_offset);
            if let Some(scrollbar) = collapsed.scrollbar {
                assert_eq!(scrollbar.max_offset, max_offset);
                assert!(scrollbar.thumb.y >= scrollbar.track.y);
                assert!(scrollbar.thumb.bottom() <= scrollbar.track.bottom());
            } else {
                assert_eq!(max_offset, 0);
            }
            assert!(!popup_text(&harness.app).contains("TOOL-OUTPUT-END"));
            focus_section(&mut harness, "recorded.tools");
            harness.key(KeyCode::Enter);
            assert!(!section_expanded(&harness, "recorded.tools"));
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some("recorded.tools")
            );
            assert!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .section_hitboxes
                    .iter()
                    .any(|(id, rect)| id == "recorded.tools" && !rect.is_empty())
            );
        }
    }
}

#[test]
fn entity_detail_t_and_space_are_consumed_without_effect_before_and_after_evidence() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = detail_harness(width, height, theme);
            harness.app.local_snapshot.tasks.clear();
            open(&mut harness);
            let initial = harness.state();
            for with_recorded in [false, true] {
                if with_recorded {
                    let data = recorded_tools_fixture(harness.app.snapshot.as_of, false);
                    harness
                        .app
                        .entity_detail
                        .as_mut()
                        .unwrap()
                        .set_recorded_details(data, theme);
                    harness.render();
                    assert!(popup_text(&harness.app).contains("Tool calls (0)"));
                    assert!(popup_text(&harness.app).contains("Usage observations (1)"));
                }
                let controls = harness.app.entity_detail_hitbox.unwrap();
                let footer = (controls.content.x..controls.back.right())
                    .map(|column| harness.cell_style(column, controls.back.y).0)
                    .collect::<String>();
                assert!(!footer.contains("[T]"));
                assert!(!footer.contains("Tools"));
                for key in ['t', 'T', ' '] {
                    let before = harness.app.entity_detail.as_ref().unwrap().expanded.clone();
                    harness.key(KeyCode::Char(key));
                    assert_eq!(harness.app.entity_detail.as_ref().unwrap().expanded, before);
                    assert_eq!(harness.state(), initial);
                    assert_eq!(harness.app.theme, theme);
                }
            }
        }
    }
}

#[test]
fn entity_detail_sections_default_to_closed_while_overview_and_usage_remain_visible() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = recorded_tools_harness(width, height, theme, true);
            let popup = harness.app.entity_detail.as_ref().unwrap();
            let foldable = section_ids(&popup.document, true);
            assert!(has_usage_node(&popup.document));
            assert!(foldable.iter().any(|id| id == "usage.calculation"));
            assert!(foldable.iter().any(|id| id == "snapshot.Related"));
            assert!(foldable.iter().any(|id| id == "snapshot.Data notes"));
            for id in &foldable {
                assert!(!popup.expanded.contains(id), "{id} defaults closed");
            }
            for title in ["Overview", "Usage"] {
                let id = popup
                    .document
                    .iter()
                    .find_map(|node| match node {
                        DetailNode::Section {
                            id,
                            title: actual,
                            foldable,
                            ..
                        } if actual == title => {
                            assert!(!foldable, "{title} remains open");
                            Some(id)
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("{title} section is retained"));
                assert!(!popup.headers.iter().any(|header| &header.id == id));
            }
            let content = popup_text(&harness.app);
            assert!(content.contains("Thread ID:"));
            assert!(content.contains("Cumulative usage (selected cycle unavailable)"));
            assert!(content.contains("Total tokens"));
            assert!(!content.contains("Approval policy: on-request"));
            assert!(!content.contains("user | commentary"));
            assert!(!content.contains("TOOL-ARGUMENTS-MARKER"));
            assert!(!content.contains("TOOL-OUTPUT-END"));
            assert!(
                !popup
                    .headers
                    .iter()
                    .any(|header| header.id.starts_with("recorded.tools/"))
            );
            assert!(popup.selected_section.is_none());
            for header in &popup.headers {
                let binding = popup.lines[header.line]
                    .spans
                    .iter()
                    .find(|span| span.content.as_ref() == "↵")
                    .expect("each interactive header displays its actual binding");
                let active = false;
                assert_eq!(binding.style.fg == Some(theme.palette().accent), active);
                assert_eq!(binding.style.add_modifier.contains(Modifier::BOLD), active);
            }

            expand_all(&mut harness);
            let expanded = popup_text(&harness.app);
            for retained in [
                "Approval policy: on-request",
                "user | commentary",
                "Message bodies: hidden",
                "TOOL-ARGUMENTS-MARKER",
                "TOOL-OUTPUT-END",
                "Usage observations (1)",
            ] {
                assert!(
                    expanded.contains(retained),
                    "retained after explicit expansion: {retained}"
                );
            }
            assert!(!expanded.contains("MESSAGE-BODY-STAYS-HIDDEN"));
        }
    }
}

#[test]
fn entity_detail_cycle_comparison_keeps_lifetime_and_calculation_details_separate() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut harness = cycle_usage_harness(160, 45, theme);
        open(&mut harness);
        let content = popup_text(&harness.app);
        assert!(content.contains("Current cycle"));
        assert!(content.contains("Own: this session"));
        assert!(content.contains("Delegated: all linked descendants"));
        assert_comparison_row(&content, "Total tokens", &["1,113", "5,566", "6,679"]);
        assert_comparison_row(&content, "TOKEN%", &["1.0000%", "5.0000%", "6.0000%"]);
        assert_comparison_row(
            &content,
            "API equivalent",
            &["$1.0000", "$5.0000", "$6.0000"],
        );
        for cumulative in ["11,130", "55,660", "66,790"] {
            assert!(
                !content.contains(cumulative),
                "lifetime is closed: {content}"
            );
        }
        assert!(!content.contains("current_codex_gauge_credit_rate_weighted_proxy"));
        let order = harness
            .app
            .entity_detail
            .as_ref()
            .unwrap()
            .headers
            .iter()
            .map(|header| header.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            &order[..3],
            ["usage.lifetime", "usage.calculation", "snapshot.Related"]
        );
        assert!(!section_expanded(&harness, "usage.lifetime"));
        assert!(!section_expanded(&harness, "usage.calculation"));

        harness.key(KeyCode::Tab);
        assert_eq!(
            harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .selected_section
                .as_deref(),
            Some("usage.lifetime")
        );
        harness.key(KeyCode::Enter);
        let expanded = popup_text(&harness.app);
        let lifetime = expanded
            .split_once("All observed cumulative tokens")
            .expect("lifetime explanation is revealed")
            .1;
        assert_comparison_row(lifetime, "Total tokens", &["11,130", "55,660", "66,790"]);
        assert!(
            expanded.contains("6,679"),
            "current-cycle data remains visible"
        );
        assert!(!section_expanded(&harness, "usage.calculation"));

        harness.key(KeyCode::Tab);
        assert_eq!(
            harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .selected_section
                .as_deref(),
            Some("usage.calculation")
        );
        harness.key(KeyCode::Enter);
        let expanded = popup_text(&harness.app);
        for explanation in [
            "Attribution method: current_codex_gauge_credit_rate_weighted_proxy",
            "External activity possible: true",
            "it does not report detected activity",
            "EST Longx: disabled",
            "same total-token denominator",
        ] {
            assert!(
                expanded.contains(explanation),
                "missing {explanation}: {expanded}"
            );
        }

        harness.key(KeyCode::Esc);
        expand_task_tree(&mut harness.app);
        harness.app.task_source_filter = TaskSourceFilter::Desktop;
        harness.render();
        open(&mut harness);
        assert_eq!(
            popup_text(&harness.app),
            content,
            "tree expansion and hidden descendant rows do not change the comparison"
        );
    }
}

#[test]
fn entity_detail_usage_comparison_gallery_preserves_large_cache_and_small_output() {
    for (theme, theme_name) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
        for (width, height) in [(160, 45), (60, 24)] {
            let mut harness = screenshot_usage_harness(width, height, theme);
            open(&mut harness);
            let content = popup_text(&harness.app);
            for label in [
                "Token counts",
                "Own",
                "Delegated",
                "Total",
                "Quota estimate",
                "API-equivalent cost",
                "Lifetime usage (same as current cycle)",
                "Usage calculation details",
            ] {
                assert!(content.contains(label), "missing {label}: {content}");
            }
            assert!(!section_expanded(&harness, "usage.lifetime"));
            assert!(!section_expanded(&harness, "usage.calculation"));
            if width >= 120 {
                assert_comparison_row(
                    &content,
                    "Total tokens",
                    &["63,950,690", "68,008,233", "131,958,923"],
                );
                assert_comparison_row(
                    &content,
                    "Uncached input",
                    &["1,762,904", "2,487,088", "4,249,992"],
                );
                assert_comparison_row(
                    &content,
                    "Cache read",
                    &["61,897,856", "65,154,688", "127,052,544"],
                );
                assert_comparison_row(
                    &content,
                    "Non-reasoning output",
                    &["163,688", "223,202", "386,890"],
                );
                assert_comparison_row(
                    &content,
                    "Reasoning output",
                    &["126,242", "143,255", "269,497"],
                );
                assert!(content.contains("96.28%"), "cache dominates the true total");
                assert!(
                    content.contains("0.29%"),
                    "small output remains precisely represented"
                );
                assert!(
                    content.contains("0.20%"),
                    "reasoning uses the same total denominator"
                );
                assert_comparison_row(&content, "Estimated quota", &["~8.1%", "~8.7%", "~16.8%"]);
                assert_comparison_row(
                    &content,
                    "API equivalent",
                    &["$24.6423+", "$62.5000", "$87.1423+"],
                );
                assert!(
                    content.contains("62,890,536"),
                    "known priced token count is retained"
                );
                assert!(
                    content.contains("501"),
                    "known priced sample count is retained"
                );
                assert!(
                    content.contains("517"),
                    "known observed sample count is retained"
                );
            } else {
                for value in [
                    "63,950,690",
                    "68,008,233",
                    "131,958,923",
                    "127,052,544",
                    "386,890",
                    "269,497",
                ] {
                    assert!(
                        content.contains(value),
                        "narrow layout preserves exact value {value}"
                    );
                }
            }
            align_usage_at_top(&mut harness);
            if width >= 120 {
                let viewport = harness.app.entity_detail_hitbox.unwrap().content;
                let visible = rendered_region(&harness, viewport);
                assert!(
                    visible.contains("API-equivalent cost"),
                    "fee fits the wide Usage view"
                );
                assert!(
                    visible.contains("Lifetime usage"),
                    "folded lifetime fits the wide Usage view"
                );
            }
            save_gallery(
                &harness,
                &format!("entity-usage-comparison-{theme_name}-{width}x{height}"),
            );
        }
    }
}

#[test]
fn entity_detail_usage_supplements_use_enter_tab_and_whole_labels_in_compact_themes() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = cycle_usage_harness(width, height, theme);
            open(&mut harness);
            for id in ["usage.lifetime", "usage.calculation"] {
                assert!(!section_expanded(&harness, id));
                let header = focus_section(&mut harness, id);
                assert_section_binding(&harness, header, true);
                let before = harness.state();
                harness.key(KeyCode::Enter);
                assert!(section_expanded(&harness, id));
                harness.key(KeyCode::Enter);
                assert!(!section_expanded(&harness, id));
                assert_eq!(harness.state(), before);
                let header = focus_section(&mut harness, id);
                for row in header.y..header.bottom() {
                    for column in header.x..header.right() {
                        let current = focus_section(&mut harness, id);
                        assert_eq!(current, header, "folded header geometry stays stable");
                        assert!(mouse_at(
                            &mut harness,
                            MouseEventKind::Down(MouseButton::Left),
                            column,
                            row
                        ));
                        assert!(
                            section_expanded(&harness, id),
                            "click {column},{row} activates {id}"
                        );
                        harness.key(KeyCode::Enter);
                        assert!(!section_expanded(&harness, id));
                    }
                }
                let smaller = if width > 32 { (32, 14) } else { (120, 40) };
                harness.resize(smaller.0, smaller.1);
                assert_eq!(
                    harness
                        .app
                        .entity_detail
                        .as_ref()
                        .unwrap()
                        .selected_section
                        .as_deref(),
                    Some(id)
                );
                let resized = focus_section(&mut harness, id);
                assert_section_binding(&harness, resized, true);
                harness.key(KeyCode::Enter);
                assert!(section_expanded(&harness, id));
                harness.key(KeyCode::Enter);
                harness.resize(width, height);
                let restored = focus_section(&mut harness, id);
                assert_section_binding(&harness, restored, true);
            }
            focus_section(&mut harness, "usage.calculation");
            harness.key(KeyCode::BackTab);
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some("usage.lifetime")
            );
            harness.key(KeyCode::Tab);
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some("usage.calculation")
            );
        }
    }
}

#[test]
fn entity_detail_tab_and_shift_tab_cycle_visible_headers_and_footer_actions() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = recorded_tools_harness(width, height, theme, true);
            let controls = harness.app.entity_detail_hitbox.unwrap();
            assert_binding(&harness, controls.next, "Tab", true);
            assert_binding(&harness, controls.toggle, "↵", false);
            let order = harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .headers
                .iter()
                .map(|header| header.id.clone())
                .collect::<Vec<_>>();
            assert!(order.len() > 2);
            assert!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .is_none()
            );
            harness.key(KeyCode::Enter);
            assert!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .is_none()
            );
            harness.key(KeyCode::Tab);
            let initial = harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .selected_section
                .clone()
                .unwrap();
            assert_eq!(initial, order[0]);
            assert_binding(&harness, controls.toggle, "↵", true);
            for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
                for key in [
                    KeyCode::Tab,
                    KeyCode::BackTab,
                    KeyCode::Enter,
                    KeyCode::Char(' '),
                ] {
                    let before = harness.app.entity_detail.as_ref().unwrap().expanded.clone();
                    handle_key_event(&mut harness.app, KeyEvent::new(key, modifiers));
                    harness.render();
                    let popup = harness.app.entity_detail.as_ref().unwrap();
                    assert_eq!(popup.selected_section.as_ref(), Some(&initial));
                    assert_eq!(popup.expanded, before);
                }
            }
            let start = order.iter().position(|id| id == &initial).unwrap();
            for step in 1..=order.len() {
                harness.key(KeyCode::Tab);
                let selected = &order[(start + step) % order.len()];
                let popup = harness.app.entity_detail.as_ref().unwrap();
                assert_eq!(popup.selected_section.as_ref(), Some(selected));
                assert!(
                    popup
                        .section_hitboxes
                        .iter()
                        .any(|(id, rect)| id == selected && !rect.is_empty()),
                    "Tab scrolls {selected} into view"
                );
            }
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_ref(),
                Some(&initial)
            );
            harness.key(KeyCode::BackTab);
            let previous = &order[(start + order.len() - 1) % order.len()];
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_ref(),
                Some(previous)
            );
            handle_key_event(
                &mut harness.app,
                KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            );
            harness.render();
            let previous = &order[(start + order.len() - 2) % order.len()];
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_ref(),
                Some(previous)
            );

            for column in controls.next.x..controls.next.right() {
                let before = harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .clone();
                assert!(click_at(&mut harness, controls.next, column));
                assert_ne!(
                    harness.app.entity_detail.as_ref().unwrap().selected_section,
                    before
                );
                assert_eq!(
                    harness.app.entity_detail_hitbox.unwrap().next,
                    controls.next
                );
            }
            let selected_header = focus_section(&mut harness, "snapshot.Related");
            assert_binding(&harness, selected_header, "↵", true);
            for column in controls.toggle.x..controls.toggle.right() {
                let before = section_expanded(&harness, "snapshot.Related");
                assert!(click_at(&mut harness, controls.toggle, column));
                assert_ne!(section_expanded(&harness, "snapshot.Related"), before);
                assert_eq!(
                    harness.app.entity_detail_hitbox.unwrap().toggle,
                    controls.toggle
                );
            }
            let before = section_expanded(&harness, "snapshot.Related");
            harness.key(KeyCode::Char(' '));
            assert_eq!(section_expanded(&harness, "snapshot.Related"), before);
            harness.key(KeyCode::Enter);
            assert_ne!(section_expanded(&harness, "snapshot.Related"), before);
            harness.key(KeyCode::Enter);
            assert_eq!(section_expanded(&harness, "snapshot.Related"), before);

            assert!(
                !harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .headers
                    .iter()
                    .any(|header| header.id.starts_with("recorded.tools/"))
            );
            focus_section(&mut harness, "recorded.tools");
            harness.key(KeyCode::Enter);
            let call = first_tool_call_id(&harness);
            assert!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .headers
                    .iter()
                    .any(|header| header.id == call)
            );
            assert!(!section_expanded(&harness, &call));
            focus_section(&mut harness, &call);
            focus_section(&mut harness, "recorded.tools");
            harness.key(KeyCode::Enter);
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some("recorded.tools")
            );
            assert!(
                !harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .headers
                    .iter()
                    .any(|header| header.id == call)
            );
        }
    }
}

#[test]
fn entity_detail_unicode_headers_keep_their_hitboxes_and_selected_identity_on_resize() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = recorded_tools_harness(width, height, theme, false);
            let title = format!("{} HEADER-END", "中文 👩‍💻 标题 ".repeat(5));
            let popup = harness.app.entity_detail.as_mut().unwrap();
            popup.document = vec![
                DetailNode::Section {
                    id: "fixture.unicode".to_string(),
                    title: title.clone(),
                    foldable: true,
                    children: vec![DetailNode::Section {
                        id: "fixture.child".to_string(),
                        title: "子项 🧑‍💻".to_string(),
                        foldable: true,
                        children: vec![DetailNode::Lines(vec![Line::from(
                            "NESTED-UNICODE-CONTENT",
                        )])],
                    }],
                },
                DetailNode::Section {
                    id: "fixture.after".to_string(),
                    title: "Following section".to_string(),
                    foldable: true,
                    children: vec![DetailNode::Lines(
                        (0..40)
                            .map(|index| Line::from(format!("after row {index}")))
                            .collect(),
                    )],
                },
            ];
            popup.expanded.clear();
            popup.selected_section = Some("fixture.unicode".to_string());
            popup.rebuild(1, theme);
            harness.render();
            assert!(popup_text(&harness.app).contains(&title));
            let rect = focus_section(&mut harness, "fixture.unicode");
            assert!(rect.right() <= harness.app.entity_detail_hitbox.unwrap().content.right());
            assert!(rect.bottom() <= harness.app.entity_detail_hitbox.unwrap().content.bottom());
            if width <= 60 {
                assert!(rect.height > 1);
            }
            for row in rect.y..rect.bottom() {
                for column in rect.x..rect.right() {
                    mouse_at(
                        &mut harness,
                        MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                    );
                    assert!(section_expanded(&harness, "fixture.unicode"));
                    assert_eq!(
                        harness
                            .app
                            .entity_detail
                            .as_ref()
                            .unwrap()
                            .selected_section
                            .as_deref(),
                        Some("fixture.unicode")
                    );
                    mouse_at(
                        &mut harness,
                        MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                    );
                    assert!(!section_expanded(&harness, "fixture.unicode"));
                }
            }
            harness.key(KeyCode::Enter);
            focus_section(&mut harness, "fixture.child");
            harness.key(KeyCode::Enter);
            assert!(popup_text(&harness.app).contains("NESTED-UNICODE-CONTENT"));
            for (resized_width, resized_height) in [(40, 24), (32, 14), (width, height)] {
                harness.resize(resized_width, resized_height);
                let popup = harness.app.entity_detail.as_ref().unwrap();
                assert_eq!(popup.selected_section.as_deref(), Some("fixture.child"));
                assert!(
                    popup
                        .section_hitboxes
                        .iter()
                        .any(|(id, area)| id == "fixture.child" && !area.is_empty())
                );
            }
            assert!(harness.app.toggle_entity_detail_section("fixture.unicode"));
            harness.render();
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some("fixture.unicode")
            );
            assert!(!popup_text(&harness.app).contains("NESTED-UNICODE-CONTENT"));
            assert!(
                !harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .headers
                    .iter()
                    .any(|header| header.id == "fixture.child")
            );
        }
    }
}

#[test]
fn entity_detail_entries_style_active_bindings_and_keep_search_geometry() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            for focus in [Focus::Tasks, Focus::Turns] {
                let mut harness = detail_harness(width, height, theme);
                if focus == Focus::Turns {
                    harness.app.focus_turns();
                }
                harness.render();
                let initial = entry(&harness, focus);
                assert_binding(&harness, initial, "F2", true);
                let other = if focus == Focus::Tasks {
                    Focus::Turns
                } else {
                    Focus::Tasks
                };
                assert_binding(&harness, entry(&harness, other), "F2", false);

                // Selecting another item preserves the entire clickable label.
                harness.key(KeyCode::Down);
                assert_eq!(entry(&harness, focus), initial);

                match focus {
                    Focus::Tasks => harness.app.begin_task_search(),
                    Focus::Turns => harness.app.begin_turn_search(),
                    _ => unreachable!(),
                }
                harness.render();
                assert_eq!(entry(&harness, focus), initial);
                assert_binding(&harness, initial, "F2", false);
                harness.key(KeyCode::Char('d'));
                assert!(harness.app.entity_detail.is_none());
                let query = if focus == Focus::Tasks {
                    &harness.app.task_search
                } else {
                    &harness.app.turn_search
                };
                assert_eq!(query, "d");
                harness.key(KeyCode::F(2));
                assert!(harness.app.entity_detail.is_none());
            }
        }
    }
}

#[test]
fn entity_detail_entries_and_popup_controls_are_whole_label_clickable() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            for focus in [Focus::Tasks, Focus::Turns] {
                let mut harness = detail_harness(width, height, theme);
                if focus == Focus::Turns {
                    harness.app.focus_turns();
                }
                harness.render();
                let control = entry(&harness, focus);
                for column in control.x..control.right() {
                    assert!(click_at(&mut harness, control, column));
                    assert!(harness.app.entity_detail.is_some());
                    harness.key(KeyCode::Esc);
                    assert!(harness.app.entity_detail.is_none());
                    assert_eq!(entry(&harness, focus), control);
                }

                // Enough independent lines to exercise both scroll buttons at every size.
                let lines: Vec<_> = (0..100)
                    .map(|index| Line::from(format!("detail row {index}")))
                    .collect();
                open_with_lines(&mut harness, lines.clone());
                let hitbox = harness.app.entity_detail_hitbox.unwrap();
                for column in hitbox.down.x..hitbox.down.right() {
                    harness.key(KeyCode::Home);
                    assert!(click_at(&mut harness, hitbox.down, column));
                    assert!(harness.app.entity_detail.as_ref().unwrap().offset > 0);
                }
                for column in hitbox.up.x..hitbox.up.right() {
                    harness.key(KeyCode::End);
                    let before = harness.app.entity_detail.as_ref().unwrap().offset;
                    assert!(click_at(&mut harness, hitbox.up, column));
                    assert!(harness.app.entity_detail.as_ref().unwrap().offset < before);
                }
                for column in hitbox.back.x..hitbox.back.right() {
                    assert!(click_at(&mut harness, hitbox.back, column));
                    assert!(harness.app.entity_detail.is_none());
                    open_with_lines(&mut harness, lines.clone());
                }
            }
        }
    }
}

#[test]
fn entity_detail_modal_scrolls_without_running_background_shortcuts() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = detail_harness(width, height, theme);
            harness.app.selected_task = 1;
            harness.app.selected_turn = 1;
            harness.app.focus_turns();
            harness.render();
            let initial = harness.state();
            let lines = (0..100)
                .map(|index| Line::from(format!("detail row {index}")))
                .collect();
            open_with_lines(&mut harness, lines);
            let controls = harness.app.entity_detail_hitbox.unwrap();
            for (area, binding, active) in [
                (controls.up, "↑", false),
                (controls.down, "↓", true),
                (controls.back, "←", true),
            ] {
                assert_binding(&harness, area, binding, active);
            }
            assert!(!harness.app.shortcuts_active());
            for code in ['1', 'U', 'T', 'R', 'V', 'd'] {
                assert!(!harness.key(KeyCode::Char(code)));
                assert_eq!(harness.app.theme, theme);
                assert_eq!(harness.state(), initial);
                assert!(!harness.app.quit_requested);
            }

            harness.key(KeyCode::Down);
            assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 1);
            harness.key(KeyCode::PageDown);
            assert!(harness.app.entity_detail.as_ref().unwrap().offset > 1);
            harness.key(KeyCode::End);
            let max_offset = harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .line_count
                .saturating_sub(usize::from(controls.content.height));
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                max_offset
            );
            assert_binding(&harness, controls.up, "↑", true);
            assert_binding(&harness, controls.down, "↓", false);
            harness.key(KeyCode::Down);
            assert_eq!(
                harness.app.entity_detail.as_ref().unwrap().offset,
                max_offset
            );
            harness.key(KeyCode::PageUp);
            assert!(harness.app.entity_detail.as_ref().unwrap().offset < max_offset);
            harness.key(KeyCode::Home);
            harness.key(KeyCode::Up);
            assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 0);
            let after = harness.app.entity_detail_hitbox.unwrap();
            assert_eq!(after.content, controls.content);
            assert_eq!(after.up, controls.up);
            assert_eq!(after.down, controls.down);
            assert_eq!(after.back, controls.back);
            harness.key(KeyCode::Left);
            assert!(harness.app.entity_detail.is_none());
            assert_eq!(harness.state(), initial);
            open(&mut harness);
            assert!(!harness.key(KeyCode::Char('q')));
            assert!(harness.app.entity_detail.is_none());
            assert!(!harness.app.quit_requested);
            assert_eq!(harness.state(), initial);
        }
    }
}

#[test]
fn entity_detail_wheel_and_drag_reach_the_last_line_at_compact_sizes() {
    for (width, height) in SIZES {
        let mut harness = detail_harness(width, height, Theme::Dark);
        let lines = (0..100)
            .map(|index| Line::from(format!("scroll detail row {index:03}")))
            .collect();
        open_with_lines(&mut harness, lines);
        let hitbox = harness.app.entity_detail_hitbox.unwrap();
        let scrollbar = hitbox.scrollbar.expect("long details require a scrollbar");
        assert_eq!(
            scrollbar.max_offset,
            100 - usize::from(hitbox.content.height)
        );
        assert_eq!(scrollbar.track.y, hitbox.content.y);
        assert!(mouse_at(
            &mut harness,
            MouseEventKind::ScrollDown,
            hitbox.content.x,
            hitbox.content.y
        ));
        assert!(harness.app.entity_detail.as_ref().unwrap().offset > 0);
        assert!(mouse_at(
            &mut harness,
            MouseEventKind::ScrollUp,
            hitbox.content.x,
            hitbox.content.y
        ));
        assert_eq!(harness.app.entity_detail.as_ref().unwrap().offset, 0);
        assert!(mouse_at(
            &mut harness,
            MouseEventKind::Down(MouseButton::Left),
            scrollbar.thumb.x,
            scrollbar.thumb.y
        ));
        assert!(mouse_at(
            &mut harness,
            MouseEventKind::Drag(MouseButton::Left),
            scrollbar.track.x,
            scrollbar.track.bottom() - 1
        ));
        mouse_at(
            &mut harness,
            MouseEventKind::Up(MouseButton::Left),
            scrollbar.track.x,
            scrollbar.track.bottom() - 1,
        );
        assert_eq!(
            harness.app.entity_detail.as_ref().unwrap().offset,
            scrollbar.max_offset
        );
        assert!(
            harness
                .frame()
                .snapshot_text()
                .contains("scroll detail row 099")
        );
        assert!(harness.app.scroll_drag.is_none());
    }
}

#[test]
fn entity_detail_keeps_full_unicode_title_id_and_path_available_by_scrolling() {
    let mut harness = detail_harness(60, 24, Theme::Dark);
    let full_title = format!("{} TITLE-END", "会话内容 👩‍💻 ".repeat(24));
    let full_id = format!("thread-{}-ID-END", "0123456789abcdef".repeat(5));
    let full_path = format!("/tmp/{}/PATH-END", "目录workspace/".repeat(14));
    let old_id = harness.app.snapshot.tasks[0].thread_id.clone();
    harness.app.snapshot.tasks[0].title = full_title.clone();
    harness.app.snapshot.tasks[0].thread_id = full_id.clone();
    harness.app.snapshot.tasks[0].cwd = Some(full_path.clone().into());
    for turn in &mut harness.app.snapshot.turns {
        if turn.thread_id == old_id {
            turn.thread_id = full_id.clone();
        }
    }
    harness.render();
    open(&mut harness);
    let source = popup_text(&harness.app);
    assert!(source.contains(&full_title));
    assert!(source.contains(&full_id));
    assert!(source.contains(&full_path));
    let mut visible = String::new();
    loop {
        let viewport = harness.app.entity_detail_hitbox.unwrap().content;
        for row in viewport.y..viewport.bottom() {
            let text = (viewport.x..viewport.right())
                .map(|column| harness.cell_style(column, row).0)
                .collect::<String>();
            visible.push_str(text.trim_end());
        }
        let before = harness.app.entity_detail.as_ref().unwrap().offset;
        harness.key(KeyCode::Down);
        if harness.app.entity_detail.as_ref().unwrap().offset == before {
            break;
        }
    }
    for marker in ["TITLE-END", "ID-END", "PATH-END"] {
        assert!(
            visible.contains(marker),
            "full {marker} must be reachable without truncation"
        );
    }
    for theme in [Theme::Dark, Theme::Light] {
        harness.app.theme = theme;
        for (width, height) in [(8, 5), (3, 2), (2, 1), (1, 1)] {
            harness.resize(width, height);
            harness.key(KeyCode::End);
            harness.key(KeyCode::Home);
            assert!(harness.app.entity_detail.is_some());
        }
    }
    harness.key(KeyCode::Esc);
    assert!(harness.app.entity_detail.is_none());
}

#[test]
fn entity_detail_message_preview_defaults_closed_and_expands_only_the_selected_user_message() {
    let message = full_preview_message();
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = preview_turn_harness(width, height, theme, &message);
            open(&mut harness);
            let popup = harness.app.entity_detail.as_ref().unwrap();
            assert!(section_ids(&popup.document, true).contains(&MESSAGE_PREVIEW_SECTION.into()));
            assert_eq!(popup.headers.first().unwrap().id, MESSAGE_PREVIEW_SECTION);
            assert!(!popup.expanded.contains(MESSAGE_PREVIEW_SECTION));
            let heading = popup.lines[popup.headers[0].line].to_string();
            assert_eq!(heading.trim(), "[↵] ▸ Message preview");
            let binding = popup.lines[popup.headers[0].line]
                .spans
                .iter()
                .find(|span| span.content.as_ref() == "↵")
                .unwrap();
            assert_ne!(binding.style.fg, Some(theme.palette().accent));
            assert!(!binding.style.add_modifier.contains(Modifier::BOLD));
            let teaser = closed_preview_teaser(&harness);
            let full_teaser = format!("  Saved preview: {}", snapshot_preview(&message));
            if UnicodeWidthStr::width(full_teaser.as_str())
                <= usize::from(harness.app.entity_detail_hitbox.unwrap().content.width)
            {
                assert_eq!(teaser, full_teaser);
            } else {
                assert!(
                    teaser.ends_with('…'),
                    "a narrow teaser is explicitly shortened"
                );
            }
            assert!(!popup_text(&harness.app).contains("Full message:"));
            assert!(!popup_text(&harness.app).contains("Full message unavailable"));
            harness.key(KeyCode::Enter);
            assert!(!section_expanded(&harness, MESSAGE_PREVIEW_SECTION));

            inject_preview_messages(&mut harness, &message);
            assert_eq!(closed_preview_teaser(&harness), teaser);
            assert!(!popup_text(&harness.app).contains("PREVIEW-FULL-END"));
            let initial = harness.state();
            harness.key(KeyCode::Tab);
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some(MESSAGE_PREVIEW_SECTION)
            );
            let heading = focus_preview_with_teaser(&mut harness);
            assert_eq!(heading.height, 1, "the preview heading stays compact");
            assert_section_binding(&harness, heading, true);
            let preview_row = heading.bottom();
            let body = harness.app.entity_detail_hitbox.unwrap().body;
            assert!(
                preview_row < body.bottom(),
                "the teaser is visible below its heading"
            );
            assert_eq!(
                rendered_row(&harness, body, preview_row).trim(),
                teaser.trim()
            );
            assert!(
                !harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .section_hitboxes
                    .iter()
                    .any(|(_, hitbox)| hitbox.y <= preview_row && preview_row < hitbox.bottom()),
                "the teaser row is not a shortcut-labelled control"
            );
            mouse_at(
                &mut harness,
                MouseEventKind::Down(MouseButton::Left),
                body.x,
                preview_row,
            );
            assert!(!section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some(MESSAGE_PREVIEW_SECTION),
                "clicking the teaser does not add a separate focus target"
            );
            harness.key(KeyCode::Enter);
            assert!(section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
            let expanded = popup_text(&harness.app);
            for included in [
                "第01段",
                "第24段",
                "VISIBLE-SAFE-TEXT",
                "PREVIEW-FULL-END",
                "👨‍👩‍👧‍👦",
                "e\u{301}",
            ] {
                assert!(
                    expanded.contains(included),
                    "full preview preserves {included}: {expanded}"
                );
            }
            for excluded in [
                "ASSISTANT-CONTENT-MUST-STAY-HIDDEN",
                "OTHER-TURN-CONTENT-MUST-STAY-HIDDEN",
                "UNASSIGNED-CONTENT-MUST-STAY-HIDDEN",
                "CONTEXT-CONTENT-MUST-STAY-HIDDEN",
            ] {
                assert!(
                    !expanded.contains(excluded),
                    "the preview must not expose {excluded}"
                );
            }
            assert!(!expanded.contains('\u{1b}'));
            assert!(!expanded.contains('\u{202e}'));
            assert!(!expanded.contains('\r'));
            assert!(!expanded.contains('\t'));
            assert_eq!(harness.state(), initial);

            focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
            harness.key(KeyCode::Enter);
            assert!(!section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
            assert_eq!(closed_preview_teaser(&harness), teaser);
            let heading = focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
            for column in heading.x..heading.right() {
                let current = focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
                assert_eq!(current, heading, "collapsed whole-label geometry is stable");
                assert!(click_at(&mut harness, current, column));
                assert!(section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
                harness.key(KeyCode::Enter);
                assert!(!section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
            }
            harness.key(KeyCode::Tab);
            assert_ne!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some(MESSAGE_PREVIEW_SECTION)
            );
            harness.key(KeyCode::BackTab);
            assert_eq!(
                harness
                    .app
                    .entity_detail
                    .as_ref()
                    .unwrap()
                    .selected_section
                    .as_deref(),
                Some(MESSAGE_PREVIEW_SECTION)
            );
            let smaller = if width > 32 { (32, 14) } else { (120, 40) };
            harness.resize(smaller.0, smaller.1);
            let resized = focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
            assert_eq!(resized.height, 1);
            assert_section_binding(&harness, resized, true);
            closed_preview_teaser(&harness);

            focus_section(&mut harness, "recorded.messages");
            harness.key(KeyCode::Enter);
            let metadata = popup_text(&harness.app);
            assert!(metadata.contains("Message bodies: hidden"));
            assert!(!metadata.contains("PREVIEW-FULL-END"));
            assert!(!metadata.contains("ASSISTANT-CONTENT-MUST-STAY-HIDDEN"));
        }
    }
}

#[test]
fn entity_detail_closed_message_preview_is_one_safe_row_and_respects_redaction() {
    let message = "SECRET-PREVIEW-TEXT is the selected user message. RECORDED-FULL-MESSAGE-END";
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            for case in [
                "unicode",
                "missing",
                "snapshot-redacted",
                "recorded-redacted",
            ] {
                let mut harness = preview_turn_harness(width, height, theme, message);
                if case == "unicode" {
                    harness.app.snapshot.turns[0].message_preview = Some(format!(
                        "家 👨‍👩‍👧‍👦 e\u{301} /tmp/中文/消息.rs\n\t\u{1b}[31m\u{202e} {} LONG-CLOSED-SNAPSHOT-END",
                        "长预览应当按显示列截断且不能另起一行。".repeat(12)
                    ));
                } else if case == "missing" {
                    harness.app.snapshot.turns[0].message_preview = None;
                } else if case == "snapshot-redacted" {
                    harness.app.local_redact_content = true;
                }
                harness.render();
                open(&mut harness);
                if case == "recorded-redacted" {
                    let mut data = recorded_preview_messages(
                        harness.app.snapshot.as_of,
                        &harness.app.snapshot.turns[0].turn_id,
                        message,
                    );
                    data.redacted = true;
                    harness
                        .app
                        .entity_detail
                        .as_mut()
                        .unwrap()
                        .set_recorded_details(data, theme);
                    harness.render();
                }
                let teaser = closed_preview_teaser(&harness);
                let content = popup_text(&harness.app);
                assert!(
                    !content.contains("Full message"),
                    "closed preview does not resolve a full message: {case}"
                );
                assert!(!content.contains("LONG-CLOSED-SNAPSHOT-END"));
                assert!(!content.contains("RECORDED-FULL-MESSAGE-END"));
                assert!(!content.contains("ASSISTANT-CONTENT-MUST-STAY-HIDDEN"));
                assert!(!content.contains("CONTEXT-CONTENT-MUST-STAY-HIDDEN"));
                match case {
                    "unicode" => {
                        assert!(teaser.ends_with('…'));
                        assert!(teaser.contains('家'));
                        if teaser.contains('👨') {
                            assert!(
                                teaser.contains("👨‍👩‍👧‍👦"),
                                "truncation preserves the whole emoji grapheme"
                            );
                        }
                        if width >= 60 {
                            assert!(teaser.contains("👨‍👩‍👧‍👦"));
                            assert!(teaser.contains("e\u{301}"));
                        }
                        if width == 120 {
                            save_gallery(
                                &harness,
                                match theme {
                                    Theme::Dark => "entity-message-preview-closed-dark-120x40",
                                    Theme::Light => "entity-message-preview-closed-light-120x40",
                                },
                            );
                        }
                    }
                    "missing" => assert!(teaser.contains("unavail")),
                    _ => {
                        assert!(teaser.contains("content"));
                        assert!(!content.contains("SECRET-PREVIEW-TEXT"));
                    }
                }
                let heading = focus_preview_with_teaser(&mut harness);
                assert_eq!(heading.height, 1);
                assert_eq!(
                    rendered_row(
                        &harness,
                        harness.app.entity_detail_hitbox.unwrap().body,
                        heading.bottom()
                    )
                    .trim(),
                    teaser.trim(),
                    "one physical teaser row at {width}x{height}: {case}"
                );
                harness.key(KeyCode::Tab);
                assert_ne!(
                    harness
                        .app
                        .entity_detail
                        .as_ref()
                        .unwrap()
                        .selected_section
                        .as_deref(),
                    Some(MESSAGE_PREVIEW_SECTION),
                    "the teaser adds no extra Tab target"
                );
            }
        }
    }
}

#[test]
fn entity_detail_message_preview_keeps_expansion_when_evidence_arrives_and_generates_gallery() {
    let message = full_preview_message();
    for (width, height, theme, name) in [
        (160, 45, Theme::Dark, "entity-message-preview-dark-160x45"),
        (60, 24, Theme::Light, "entity-message-preview-light-60x24"),
    ] {
        let mut harness = preview_turn_harness(width, height, theme, &message);
        open(&mut harness);
        focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
        harness.key(KeyCode::Enter);
        let fallback = popup_text(&harness.app);
        assert!(fallback.contains(&snapshot_preview(&message)));
        assert!(fallback.contains("Full message unavailable"));
        assert!(!fallback.contains("PREVIEW-FULL-END"));
        inject_preview_messages(&mut harness, &message);
        assert!(section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
        assert_eq!(
            harness
                .app
                .entity_detail
                .as_ref()
                .unwrap()
                .selected_section
                .as_deref(),
            Some(MESSAGE_PREVIEW_SECTION)
        );
        assert!(popup_text(&harness.app).contains("PREVIEW-FULL-END"));
        let heading = focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
        let body = harness.app.entity_detail_hitbox.unwrap().body;
        for _ in 0..heading.y.saturating_sub(body.y) {
            harness.key(KeyCode::Down);
        }
        save_gallery(&harness, name);
        let mut seen_tail = false;
        let limit = harness.app.entity_detail.as_ref().unwrap().line_count;
        for _ in 0..=limit {
            if rendered_region(&harness, harness.app.entity_detail_hitbox.unwrap().body)
                .contains("PREVIEW-FULL-END")
            {
                seen_tail = true;
                break;
            }
            let before = harness.app.entity_detail.as_ref().unwrap().offset;
            harness.key(KeyCode::Down);
            if harness.app.entity_detail.as_ref().unwrap().offset == before {
                break;
            }
        }
        assert!(
            seen_tail,
            "the long preview's final line is reachable at {width}x{height}"
        );
    }
}

#[test]
fn entity_detail_message_preview_falls_back_for_missing_stale_ambiguous_and_redacted_messages() {
    use crate::session_details::{DetailMessage, SessionDetails};

    let message = full_preview_message();
    for case in [
        "missing",
        "stale",
        "other-turn",
        "assistant",
        "ambiguous",
        "redacted",
    ] {
        let mut harness = preview_turn_harness(80, 24, Theme::Dark, &message);
        open(&mut harness);
        let captured = harness.app.snapshot.as_of;
        let turn_id = harness.app.snapshot.turns[0].turn_id.clone();
        let mut data = SessionDetails::default();
        data.files_read = 1;
        data.redacted = case == "redacted";
        if case != "missing" {
            data.messages.push(DetailMessage {
                role: if case == "assistant" {
                    "assistant"
                } else {
                    "user"
                }
                .into(),
                text: if case == "stale" {
                    "STALE-MESSAGE-MUST-NOT-APPEAR".into()
                } else {
                    message.clone()
                },
                timestamp: Some(captured - ChronoDuration::seconds(1)),
                turn_id: Some(
                    if case == "other-turn" {
                        "other-turn"
                    } else {
                        &turn_id
                    }
                    .into(),
                ),
                phase: None,
            });
        }
        if case == "ambiguous" {
            let mut alternative = data.messages[0].clone();
            alternative
                .text
                .push_str("\nDIFFERENT-BODY-WITH-SAME-PREVIEW");
            data.messages.push(alternative);
        }
        harness
            .app
            .entity_detail
            .as_mut()
            .unwrap()
            .set_recorded_details(data, Theme::Dark);
        harness.render();
        focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
        harness.key(KeyCode::Enter);
        let content = popup_text(&harness.app);
        if case == "redacted" {
            assert!(
                !content.contains(&snapshot_preview(&message)),
                "redaction suppresses the saved preview too"
            );
            assert!(content.contains("content redacted"));
        } else {
            assert!(
                content.contains(&snapshot_preview(&message)),
                "snapshot fallback retained: {case}"
            );
        }
        assert!(
            content.contains("Full message unavailable"),
            "explicit fallback: {case}"
        );
        assert!(
            !content.contains("PREVIEW-FULL-END"),
            "no fabricated full message: {case}"
        );
        assert!(!content.contains("STALE-MESSAGE-MUST-NOT-APPEAR"));
        assert!(!content.contains("DIFFERENT-BODY-WITH-SAME-PREVIEW"));
    }
}

#[test]
fn entity_detail_turn_shows_exact_token_components_and_api_pricing_coverage() {
    for (width, height, theme, gallery_name) in [
        (120, 40, Theme::Dark, "entity-detail-dark-120x40"),
        (60, 24, Theme::Light, "entity-detail-light-60x24"),
    ] {
        let mut harness = detail_harness(width, height, theme);
        let usage = TokenUsage {
            input_tokens: 1_234,
            cached_input_tokens: 234,
            cache_write_input_tokens: 135,
            output_tokens: 432,
            reasoning_output_tokens: 123,
            unclassified_tokens: 34,
            total_tokens: 1_700,
        };
        harness.app.snapshot.turns[0].token_usage = usage;
        harness.app.snapshot.turns[0].service_tier = Some("fast".to_string());
        harness.app.snapshot.turns[0].message_preview = Some("visible user preview".to_string());
        add_window_analysis(&mut harness.app, WindowScope::FiveHours, 1_700, 40.0);
        let analysis = &mut harness.app.snapshot.window_analyses[0];
        analysis.turns[0].usage.token_usage = usage;
        analysis.turns[0].usage.api_equivalent_cost = ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(1_000_000_000_000),
            maximum_pico_usd: PicoUsd::new(2_000_000_000_000),
            observed_samples: 4,
            priced_samples: 2,
            observed_tokens: 1_700,
            priced_tokens: 850,
        };
        harness.app.local_snapshot.tasks.clear();
        harness.app.focus_turns();
        harness.render();
        open(&mut harness);
        let content = popup_text(&harness.app);
        for text in [
            "turn-0-0",
            "model-0",
            "high",
            "fast",
            "Uncached input",
            "Cache write",
            "Cache read",
            "Non-reasoning output",
            "Reasoning output",
            "135",
            "234",
            "123",
            "API equivalent",
            "Priced token coverage",
            "50.00%",
            "Priced / observed samples",
            "50.88%",
            "13.76%",
            "7.94%",
            "18.18%",
            "7.24%",
            "2.00%",
            "865",
            "309",
        ] {
            assert!(content.contains(text), "missing {text:?}: {content}");
        }
        assert!(closed_preview_teaser(&harness).contains("visible user preview"));
        assert!(!section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
        assert_eq!(
            harness.app.entity_detail.as_ref().unwrap().headers[0].id,
            MESSAGE_PREVIEW_SECTION
        );
        let headers = content
            .lines()
            .filter(|line| line.contains("Metric"))
            .collect::<Vec<_>>();
        assert!(
            !headers.is_empty(),
            "the turn comparison has a rendered column header"
        );
        for header in headers {
            assert!(
                !header.contains("Delegated"),
                "a turn has only its Own column"
            );
        }
        assert!(
            content
                .contains("Delegated turn usage: unavailable (no exact turn linkage in snapshot)")
        );
        assert!(content.contains("% of Own"));
        assert!(!content.contains("% of Total"));
        assert!(content.contains("Lifetime usage (same as current cycle)"));
        assert!(!section_expanded(&harness, "usage.lifetime"));
        assert!(!section_expanded(&harness, "usage.calculation"));
        let directory = gallery_directory();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(format!("{gallery_name}.svg")),
            harness.frame().to_svg(gallery_name),
        )
        .unwrap();
        // Model a finished evidence load: its folded roots provide enough
        // trailing content for the Usage heading to reach the viewport top.
        let data = recorded_tools_fixture(harness.app.snapshot.as_of, false);
        harness
            .app
            .entity_detail
            .as_mut()
            .unwrap()
            .set_recorded_details(data, theme);
        harness.render();
        assert!(has_usage_node(
            &harness.app.entity_detail.as_ref().unwrap().document
        ));
        let limit = harness.app.entity_detail.as_ref().unwrap().line_count;
        let mut usage_visible = false;
        harness.key(KeyCode::Home);
        for _ in 0..=limit {
            let content = harness.app.entity_detail_hitbox.unwrap().content;
            let first_row = (content.x..content.right())
                .map(|column| harness.cell_style(column, content.y).0)
                .collect::<String>();
            if first_row.trim() == "Usage" {
                usage_visible = true;
                break;
            }
            let before = harness.app.entity_detail.as_ref().unwrap().offset;
            harness.key(KeyCode::Down);
            if harness.app.entity_detail.as_ref().unwrap().offset == before {
                break;
            }
        }
        assert!(
            usage_visible,
            "the actual Usage block remains reachable at {width}x{height}"
        );
        let usage_name = gallery_name.replace("entity-detail", "entity-usage");
        std::fs::write(
            directory.join(format!("{usage_name}.svg")),
            harness.frame().to_svg(&usage_name),
        )
        .unwrap();
    }
}

#[test]
fn entity_detail_derived_turn_statistics_keep_duration_and_count_meanings_distinct() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    let turns = &mut harness.app.snapshot.turns[..2];
    turns[0].status = TurnStatus::Completed;
    turns[0].duration_ms = Some(2_345);
    turns[1].status = TurnStatus::Failed;
    turns[1].duration_ms = Some(9_876);
    turns[1].token_usage = TokenUsage {
        input_tokens: 4_321,
        total_tokens: 4_321,
        ..TokenUsage::default()
    };
    harness.render();
    open_expanded(&mut harness);
    let content = popup_text(&harness.app);
    for text in [
        "Turns completed: 1",
        "Turns failed: 1",
        "Completed turn duration sum: 2345 ms",
        "Largest known turn: turn-0-1 (4321 tokens)",
        "Longest completed turn: turn-0-0 (2345 ms",
    ] {
        assert!(
            content.contains(text),
            "missing statistic {text:?}: {content}"
        );
    }
    assert!(!content.contains("Completed turn duration sum: 12221"));
    harness.key(KeyCode::Esc);
    let captured_at = harness.app.snapshot.as_of;
    harness.app.snapshot.turns[0].status = TurnStatus::InProgress;
    harness.app.snapshot.turns[0].started_at =
        Some(captured_at - ChronoDuration::milliseconds(3_210));
    harness.app.snapshot.turns[0].completed_at = None;
    harness.app.snapshot.turns[0].duration_ms = None;
    harness.app.snapshot.turns[0].token_usage = TokenUsage::default();
    harness.app.focus_turns();
    harness.render();
    open_expanded(&mut harness);
    let content = popup_text(&harness.app);
    assert!(content.contains("Elapsed at capture: 3210 ms"));
    assert!(content.contains("unavailable"));
    assert!(
        content.contains("zero"),
        "zero totals have no composition percentages"
    );
    for component in [
        "Total tokens",
        "Uncached input",
        "Cache read",
        "Cache write",
        "Non-reasoning output",
        "Reasoning output",
    ] {
        assert_comparison_row(&content, component, &["0"]);
    }
}

#[test]
fn entity_detail_longx_changes_quota_projection_without_changing_api_cost() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    add_window_analysis(&mut harness.app, WindowScope::FiveHours, 111, 33.0);
    let base = &mut harness.app.snapshot.window_analyses[0];
    base.turns[0].usage.api_equivalent_cost = exact_api_cost(1_000_000_000_000);
    let mut alternative = base.clone();
    alternative.turns[0].usage.estimated_quota_percent = 99.0;
    alternative.turns[0].usage.api_equivalent_cost = exact_api_cost(9_000_000_000_000);
    base.api_long_context = Some(Box::new(alternative));
    harness.app.focus_turns();
    harness.render();
    open(&mut harness);
    let without_longx = popup_text(&harness.app);
    assert_comparison_row(&without_longx, "API equivalent", &["$1.0000"]);
    harness.key(KeyCode::Esc);
    harness.app.api_long_context_multiplier = true;
    harness.render();
    open(&mut harness);
    let with_longx = popup_text(&harness.app);
    assert_comparison_row(&with_longx, "API equivalent", &["$1.0000"]);
    assert!(!with_longx.contains("$9.0000"));
    let quota = |content: &str| {
        content
            .lines()
            .find(|line| line.contains("Estimated quota"))
            .expect("the selected scope retains its estimate")
            .to_string()
    };
    assert_ne!(quota(&without_longx), quota(&with_longx));
}

#[test]
fn entity_detail_preserves_desktop_filter_binding_and_requires_unmodified_f2() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    harness.app.task_source_filter = TaskSourceFilter::All;
    harness.key(KeyCode::Char('d'));
    assert_eq!(harness.app.task_source_filter, TaskSourceFilter::Desktop);
    assert!(harness.app.entity_detail.is_none());
    for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        assert!(!handle_key_event(
            &mut harness.app,
            KeyEvent::new(KeyCode::F(2), modifiers)
        ));
        assert!(harness.app.entity_detail.is_none());
    }
    open(&mut harness);
    assert_eq!(harness.app.task_source_filter, TaskSourceFilter::Desktop);
}

#[test]
fn entity_detail_session_aggregation_is_stable_when_subagent_tree_expands() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    set_task_parent(&mut harness.app, 1, 0);
    set_task_parent(&mut harness.app, 2, 1);
    for (task, total) in harness.app.snapshot.tasks.iter_mut().zip([113, 229, 337]) {
        task.token_usage = TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..TokenUsage::default()
        };
    }
    harness.app.task_list_mode = TaskListMode::Tree;
    harness.app.selected_task = 0;
    harness.render();
    open(&mut harness);
    let collapsed = popup_text(&harness.app);
    assert!(collapsed.contains("Cumulative usage (selected cycle unavailable)"));
    assert_comparison_row(&collapsed, "Total tokens", &["113", "566", "679"]);
    assert!(
        !section_ids(&harness.app.entity_detail.as_ref().unwrap().document, true)
            .iter()
            .any(|id| id == "usage.lifetime")
    );
    assert!(!collapsed.contains("Quota estimate"));
    assert!(!collapsed.contains("API-equivalent cost"));
    harness.key(KeyCode::Esc);
    expand_task_tree(&mut harness.app);
    harness.render();
    open(&mut harness);
    assert_eq!(
        popup_text(&harness.app),
        collapsed,
        "details use stable own/delegated totals"
    );
}

fn historical_summary_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness = detail_harness(width, height, theme);
    harness.app.snapshot.tasks.clear();
    harness.app.snapshot.turns.clear();
    let starts_at = harness.app.snapshot.as_of - ChronoDuration::hours(1);
    let groups = [
        ("historical-root", None, Some("historical-turn"), 101),
        ("historical-root", None, None, 202),
        ("historical-child", Some("historical-root"), None, 303),
    ]
    .into_iter()
    .map(|(thread, parent, turn, total)| LocalProjectUsageGroup {
        thread_id: thread.to_string(),
        turn_id: turn.map(str::to_string),
        parent_thread_id: parent.map(str::to_string),
        session_thread_id: Some("historical-root".to_string()),
        session_turn_id: turn.map(str::to_string),
        message_preview: turn.map(|_| "Historical prompt".to_string()),
        turn_started_at: turn.map(|_| starts_at),
        project_id: Some("historical-project".to_string()),
        project_label: Some("history-workspace".to_string()),
        title: Some("Historical title".to_string()),
        source: Some(
            if parent.is_some() {
                "subagent"
            } else {
                "desktop"
            }
            .to_string(),
        ),
        token_usage: TokenUsage {
            input_tokens: total,
            total_tokens: total,
            ..TokenUsage::default()
        },
        call_count: 1,
        ..LocalProjectUsageGroup::default()
    })
    .collect::<Vec<_>>();
    harness.app.history.half_hour_buckets = vec![LocalHalfHourBucket {
        starts_at,
        ends_at: starts_at + ChronoDuration::minutes(15),
        sampled_at: starts_at + ChronoDuration::minutes(15),
        token_usage: TokenUsage {
            input_tokens: 606,
            total_tokens: 606,
            ..TokenUsage::default()
        },
        estimated_cost_units: 0,
        api_long_context_extra_cost_units: Some(0),
        long_context_usage_unknown: false,
        estimator_revision: crate::history::HISTORY_ESTIMATOR_REVISION,
        project_breakdown_revision: crate::history::HISTORY_PROJECT_BREAKDOWN_REVISION,
        api_pricing_catalog_revision: crate::api_cost::API_PRICING_CATALOG_REVISION,
        call_count: 3,
        groups: Vec::new(),
        project_groups: groups,
        partial_reasons: Vec::new(),
    }]
    .into();
    harness.app.summary_range = SummaryRange::SevenDays;
    harness.app.summary_cache = None;
    harness.app.set_view(View::Summary);
    harness.render();
    harness
}

#[test]
fn entity_detail_summary_message_preview_requires_an_exact_turn_and_keeps_unassigned_private() {
    let message = full_preview_message();
    let saved = snapshot_preview(&message);
    for (width, height, theme) in [(120, 40, Theme::Dark), (32, 14, Theme::Light)] {
        let mut harness = historical_summary_harness(width, height, theme);
        harness.app.history.half_hour_buckets[0].project_groups[0].message_preview =
            Some(saved.clone());
        harness.app.summary_cache = None;
        harness.render();
        let project = harness
            .app
            .summary_rows()
            .into_iter()
            .find(|row| row.kind == SummaryRowKind::Project)
            .unwrap()
            .id;
        harness.app.summary_expanded_nodes.insert(project);
        harness.render();
        let session = harness
            .app
            .summary_rows()
            .into_iter()
            .find(|row| row.kind == SummaryRowKind::Session)
            .unwrap()
            .id;
        harness.app.summary_expanded_nodes.insert(session.clone());
        harness.app.summary_selected_id = Some(session);
        harness.render();
        open(&mut harness);
        assert!(
            !section_ids(&harness.app.entity_detail.as_ref().unwrap().document, true)
                .contains(&MESSAGE_PREVIEW_SECTION.into()),
            "session details do not add a message-body control"
        );
        harness.key(KeyCode::Esc);
        let rows = harness.app.summary_rows();
        let exact = rows
            .iter()
            .find(|row| row.kind == SummaryRowKind::Turn && row.label == saved)
            .unwrap()
            .id
            .clone();
        let unassigned = rows
            .iter()
            .find(|row| row.kind == SummaryRowKind::Turn && row.label.starts_with("Unassigned"))
            .unwrap()
            .id
            .clone();
        for (selected, has_exact_turn) in [(exact, true), (unassigned, false)] {
            harness.app.summary_selected_id = Some(selected);
            harness.render();
            open(&mut harness);
            assert!(!section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
            assert!(!popup_text(&harness.app).contains("PREVIEW-FULL-END"));
            let data =
                recorded_preview_messages(harness.app.snapshot.as_of, "historical-turn", &message);
            harness
                .app
                .entity_detail
                .as_mut()
                .unwrap()
                .set_recorded_details(data, theme);
            harness.render();
            focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
            harness.key(KeyCode::Enter);
            let content = popup_text(&harness.app);
            if has_exact_turn {
                assert!(content.contains("Turn ID: historical-turn"));
                assert!(content.contains("Full message from local log:"));
                assert!(content.contains("PREVIEW-FULL-END"));
            } else {
                assert!(content.contains("Turn ID: unavailable"));
                assert!(content.contains("Full message unavailable"));
                assert!(!content.contains("PREVIEW-FULL-END"));
                assert!(!content.contains(&saved));
            }
            for excluded in [
                "ASSISTANT-CONTENT-MUST-STAY-HIDDEN",
                "OTHER-TURN-CONTENT-MUST-STAY-HIDDEN",
                "CONTEXT-CONTENT-MUST-STAY-HIDDEN",
            ] {
                assert!(!content.contains(excluded));
            }
            harness.key(KeyCode::Esc);
        }
    }
}

#[test]
fn entity_detail_summary_requires_entity_selection_and_preserves_history_only_metadata() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = historical_summary_harness(width, height, theme);
            let rows = harness.app.summary_rows();
            let project = rows
                .iter()
                .find(|row| row.kind == SummaryRowKind::Project)
                .unwrap();
            harness.app.summary_selected_id = Some(project.id.clone());
            harness.render();
            let entry = harness.app.summary_controls_hitbox.unwrap().details;
            assert_binding(&harness, entry, "F2", false);
            harness.key(KeyCode::F(2));
            assert!(harness.app.entity_detail.is_none());
            click_at(&mut harness, entry, entry.right() - 1);
            assert!(harness.app.entity_detail.is_none());

            harness
                .app
                .summary_expanded_nodes
                .insert(project.id.clone());
            harness.render();
            let rows = harness.app.summary_rows();
            let session = rows
                .iter()
                .find(|row| row.kind == SummaryRowKind::Session)
                .unwrap();
            let session_id = session.id.clone();
            harness.app.summary_selected_id = Some(session_id.clone());
            harness.render();
            assert_eq!(harness.app.summary_controls_hitbox.unwrap().details, entry);
            assert_binding(&harness, entry, "F2", true);
            open_expanded(&mut harness);
            let content = popup_text(&harness.app);
            assert!(content.contains("Historical title"));
            assert!(content.contains("historical-root"));
            assert!(content.contains("Status: unavailable"));
            assert!(content.contains("606"));
            assert!(
                content.contains("Largest known user turn (range): historical-turn (101 tokens)")
            );
            assert!(content.contains("Largest priced user-turn subtotal (range): unavailable"));
            harness.key(KeyCode::Esc);
            assert_eq!(
                harness.app.summary_selected_id.as_deref(),
                Some(session_id.as_str())
            );

            harness.app.summary_expanded_nodes.insert(session_id);
            harness.render();
            let rows = harness.app.summary_rows();
            for row in rows.iter().filter(|row| row.kind == SummaryRowKind::Turn) {
                harness.app.summary_selected_id = Some(row.id.clone());
                harness.render();
                open_expanded(&mut harness);
                let content = popup_text(&harness.app);
                assert!(content.contains("Model: unavailable"));
                if row.label.starts_with("Unassigned") {
                    assert!(content.contains(&row.label));
                    assert!(content.contains("Turn ID: unavailable"));
                    assert!(!content.contains("Turn ID: historical-turn"));
                } else {
                    assert!(content.contains("historical-turn"));
                    assert!(content.contains("Historical prompt"));
                }
                harness.key(KeyCode::Esc);
            }
        }
    }
}

#[test]
fn entity_detail_summary_rankings_exclude_unassigned_and_unpriced_turns() {
    let mut harness = historical_summary_harness(80, 24, Theme::Dark);
    let bucket = &mut harness.app.history.half_hour_buckets[0];
    bucket.project_groups[0].turn_started_at = Some(bucket.starts_at - ChronoDuration::hours(1));
    bucket.project_groups[0].api_equivalent_cost = ApiCostAmount {
        observed_samples: 1,
        observed_tokens: 101,
        ..ApiCostAmount::default()
    };
    // This unassigned row is larger and more recent than either known user turn.
    bucket.project_groups[1].turn_started_at = Some(bucket.starts_at);
    let latest_known = bucket.starts_at - ChronoDuration::minutes(5);
    let mut priced_zero = bucket.project_groups[0].clone();
    priced_zero.turn_id = Some("priced-zero-turn".to_string());
    priced_zero.session_turn_id = priced_zero.turn_id.clone();
    priced_zero.turn_started_at = Some(latest_known);
    priced_zero.message_preview = Some("Known zero-priced user turn".to_string());
    priced_zero.token_usage = TokenUsage {
        input_tokens: 55,
        total_tokens: 55,
        ..TokenUsage::default()
    };
    priced_zero.api_equivalent_cost = ApiCostAmount {
        observed_samples: 1,
        priced_samples: 1,
        observed_tokens: 55,
        priced_tokens: 55,
        ..ApiCostAmount::default()
    };
    bucket.token_usage.add_assign(priced_zero.token_usage);
    bucket.call_count += 1;
    bucket.project_groups.push(priced_zero);
    harness.app.summary_cache = None;
    harness.render();
    let rows = harness.app.summary_rows();
    let project = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Project)
        .unwrap();
    harness
        .app
        .summary_expanded_nodes
        .insert(project.id.clone());
    harness.render();
    let rows = harness.app.summary_rows();
    let session = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Session)
        .unwrap();
    harness.app.summary_selected_id = Some(session.id.clone());
    harness.render();
    open_expanded(&mut harness);
    let content = popup_text(&harness.app);
    assert!(content.contains("Largest known user turn (range): historical-turn (101 tokens)"));
    assert!(content.contains("Largest priced user-turn subtotal (range): priced-zero-turn ("));
    assert!(!content.contains("Largest priced user-turn subtotal (range): historical-turn"));
    assert!(content.contains(&format!(
        "Latest known user-turn start: {}",
        latest_known.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S %:z")
    )));
}

#[test]
fn entity_detail_summary_uses_selected_project_when_one_session_spans_projects() {
    let mut harness = historical_summary_harness(120, 40, Theme::Dark);
    let bucket = &mut harness.app.history.half_hour_buckets[0];
    for (index, group) in bucket.project_groups.iter_mut().enumerate() {
        // Project grouping belongs to emitting threads. Two branches can share
        // one root session while retaining their distinct observed projects.
        group.thread_id = format!("historical-emitter-{index}");
        group.parent_thread_id = Some("historical-root".to_string());
        group.source = Some("subagent".to_string());
        group.project_id = Some(if index == 0 { "a-small" } else { "z-large" }.to_string());
        group.project_label = group.project_id.clone();
        group.api_equivalent_cost = exact_api_cost(if index == 0 {
            100_000_000_000_000
        } else {
            1_000_000_000_000
        });
    }
    harness.app.summary_metric = SummaryMetric::ApiEquivalent;
    harness.app.summary_cache = None;
    harness.render();
    let projects = &harness
        .app
        .summary_cache
        .as_ref()
        .unwrap()
        .prepared
        .usage
        .projects;
    assert_eq!(
        projects.len(),
        2,
        "two emitting projects share the same root session"
    );
    assert!(projects.iter().all(|project| {
        project
            .sessions
            .iter()
            .any(|session| session.thread_id == "historical-root")
    }));
    assert_eq!(
        projects[0].label, "z-large",
        "persisted summary is ordered by tokens"
    );
    let rows = harness.app.summary_rows();
    let project = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Project)
        .unwrap();
    assert_eq!(
        project.label, "a-small",
        "the displayed tree is ordered by API amount"
    );
    harness
        .app
        .summary_expanded_nodes
        .insert(project.id.clone());
    harness.render();
    let rows = harness.app.summary_rows();
    let session = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Session)
        .unwrap();
    assert_eq!(session.metrics.token_usage.total_tokens, 101);
    harness.app.summary_selected_id = Some(session.id.clone());
    harness.render();
    open(&mut harness);
    let content = popup_text(&harness.app);
    assert!(content.contains("Project: a-small"));
    assert!(content.contains("Total range tokens: 101"));
    assert!(!content.contains("Project: z-large"));
    assert!(!content.contains("Total range tokens: 505"));
}

#[test]
fn entity_detail_local_enrichment_updates_the_modal_and_keeps_remote_content_separate() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut snapshot = interaction_test_app(1, 1).snapshot;
    snapshot.codex_home = directory.path().to_path_buf();
    let captured = snapshot.as_of;
    let thread_id = snapshot.tasks[0].thread_id.clone();
    let turn_id = snapshot.turns[0].turn_id.clone();
    let record = |timestamp: DateTime<Utc>, kind: &str, payload: serde_json::Value| {
        serde_json::json!({"timestamp": timestamp, "type": kind, "payload": payload}).to_string()
    };
    let full_message = format!(
        "{} COMPLETE-LOCAL-MESSAGE",
        "long recorded message ".repeat(20)
    );
    snapshot.turns[0].message_preview = Some(snapshot_preview(&full_message));
    let log = [
        record(captured - ChronoDuration::seconds(2), "session_meta", serde_json::json!({"id": thread_id})),
        record(captured - ChronoDuration::seconds(1), "event_msg", serde_json::json!({"type": "user_message", "turn_id": turn_id, "message": full_message})),
        record(captured + ChronoDuration::seconds(1), "event_msg", serde_json::json!({"type": "user_message", "turn_id": turn_id, "message": "FUTURE-MESSAGE-MUST-NOT-APPEAR"})),
    ].join("\n");
    std::fs::write(sessions.join(format!("rollout-{thread_id}.jsonl")), log).unwrap();
    let mut harness = TuiHarness::from_snapshot(snapshot.clone(), 80, 24, Theme::Dark);
    harness.app.focus_turns();
    harness.render();
    open_expanded(&mut harness);
    assert!(popup_text(&harness.app).contains("Recorded details: loading"));
    assert!(popup_text(&harness.app).contains("Full message: loading local log..."));
    assert!(section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
    focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
    let now = Instant::now();
    harness.app.last_local_refresh = now;
    assert_eq!(
        next_run_loop_poll_timeout(&harness.app, now, false),
        BACKGROUND_CHANNEL_POLL
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !harness.app.poll_entity_detail() {
        assert!(
            Instant::now() < deadline,
            "local enrichment did not complete"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    harness.render();
    expand_all(&mut harness);
    let content = popup_text(&harness.app);
    assert!(!harness.app.entity_detail_loading());
    assert!(content.contains("Messages (1)"));
    assert!(content.contains("Message bodies: hidden"));
    assert!(content.contains(&format!(
        "user | phase unrecorded | {} | turn {turn_id}",
        (captured - ChronoDuration::seconds(1)).to_rfc3339()
    )));
    assert!(
        content.contains(&full_message),
        "the matching current-turn preview receives its full body"
    );
    assert!(section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
    assert_eq!(
        harness
            .app
            .entity_detail
            .as_ref()
            .unwrap()
            .selected_section
            .as_deref(),
        Some(MESSAGE_PREVIEW_SECTION)
    );
    assert!(!content.contains("FUTURE-MESSAGE-MUST-NOT-APPEAR"));
    assert!(harness.app.entity_detail.is_some());
    harness.key(KeyCode::Esc);
    let now = Instant::now();
    harness.app.last_local_refresh = now;
    assert!(next_run_loop_poll_timeout(&harness.app, now, false) > BACKGROUND_CHANNEL_POLL);

    snapshot.tasks[0].source = Some("remote:other-machine".to_string());
    let mut remote = TuiHarness::from_snapshot(snapshot, 80, 24, Theme::Dark);
    remote.app.focus_turns();
    remote.render();
    open_expanded(&mut remote);
    assert!(!remote.app.poll_entity_detail());
    assert!(popup_text(&remote.app).contains("no unambiguous local rollout association"));
    assert!(popup_text(&remote.app).contains("Full message unavailable"));
    assert!(popup_text(&remote.app).contains(&snapshot_preview(&full_message)));
    assert!(!popup_text(&remote.app).contains("COMPLETE-LOCAL-MESSAGE"));
}

#[test]
fn entity_detail_message_preview_survives_an_exhausted_tool_retention_budget() {
    const TOOL_FIELD_BYTES: usize = 64 * 1024;
    const TOOL_COUNT: usize = 32;
    const FULL_END: &str = "SHORT-PREVIEW-AFTER-TOOLS-END";

    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut snapshot = interaction_test_app(1, 1).snapshot;
    snapshot.codex_home = directory.path().to_owned();
    let captured = snapshot.as_of;
    let thread_id = snapshot.tasks[0].thread_id.clone();
    let turn_id = snapshot.turns[0].turn_id.clone();
    let message = format!(
        "{}{FULL_END}",
        "请完整显示这条短用户消息。"
            .chars()
            .cycle()
            .take(90 - FULL_END.chars().count())
            .collect::<String>()
    );
    assert_eq!(message.chars().count(), 90);
    snapshot.turns[0].message_preview = Some(snapshot_preview(&message));
    let record = |timestamp: DateTime<Utc>, kind: &str, payload: serde_json::Value| {
        serde_json::json!({"timestamp": timestamp, "type": kind, "payload": payload}).to_string()
    };
    let mut records = vec![record(
        captured - ChronoDuration::seconds(4),
        "session_meta",
        serde_json::json!({"id": thread_id}),
    )];
    // All calls belong to the selected turn. The failure must be caused by
    // retained-content exhaustion, independent of other-turn filtering.
    for index in 0..TOOL_COUNT {
        records.push(record(
            captured - ChronoDuration::seconds(3),
            "response_item",
            serde_json::json!({
                "type": "function_call", "call_id": format!("budget-{index:02}"),
                "turn_id": turn_id, "name": format!("retained-budget-tool-{index:02}"),
                "arguments": "x".repeat(TOOL_FIELD_BYTES),
            }),
        ));
    }
    assert_eq!(TOOL_COUNT * TOOL_FIELD_BYTES, 2 * 1024 * 1024);
    records.push(record(
        captured - ChronoDuration::seconds(2),
        "event_msg",
        serde_json::json!({
            "type": "item_completed", "thread_id": thread_id, "turn_id": turn_id,
            "item": {"type": "UserMessage", "id": "short-selected-message", "content": [{"text": message}]},
        }),
    ));
    records.push(record(
        captured - ChronoDuration::seconds(1),
        "response_item",
        serde_json::json!({
            "type": "function_call", "call_id": "late-budget-call", "turn_id": turn_id,
            "name": "late-budget-tool", "arguments": "LATE-TOOL-ARGUMENTS-MUST-STAY-OMITTED",
        }),
    ));
    records.push(record(
        captured - ChronoDuration::seconds(1),
        "response_item",
        serde_json::json!({
            "type": "function_call_output", "call_id": "late-budget-call", "turn_id": turn_id,
            "output": "LATE-TOOL-OUTPUT-MUST-STAY-OMITTED", "exit_code": 7, "duration_ms": 125,
        }),
    ));
    assert!(records.iter().all(|line| line.len() < 2 * 1024 * 1024));
    let log = records.join("\n");
    assert!(log.len() < 3 * 1024 * 1024, "the source stays bounded");
    std::fs::write(sessions.join(format!("rollout-{thread_id}.jsonl")), log).unwrap();

    let mut harness = TuiHarness::from_snapshot(snapshot, 80, 24, Theme::Dark);
    harness.app.focus_turns();
    harness.render();
    open(&mut harness);
    closed_preview_teaser(&harness);
    focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
    harness.key(KeyCode::Enter);
    assert!(popup_text(&harness.app).contains("Full message: loading local log..."));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !harness.app.poll_entity_detail() {
        assert!(
            Instant::now() < deadline,
            "the bounded budget-exhaustion reader did not finish"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    harness.render();
    let content = popup_text(&harness.app);
    assert!(section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
    assert_eq!(
        harness
            .app
            .entity_detail
            .as_ref()
            .unwrap()
            .selected_section
            .as_deref(),
        Some(MESSAGE_PREVIEW_SECTION)
    );
    assert!(content.contains("Full message from local log:"));
    assert!(
        content.contains(&message),
        "a short selected message is not starved by tool bodies"
    );
    assert!(!content.contains("Full message unavailable"));
    assert!(content.contains("Tool calls (33)"));
    focus_section(&mut harness, "recorded.source");
    harness.key(KeyCode::Enter);
    assert!(
        popup_text(&harness.app).contains("2 MiB retained-content limit"),
        "the original shared budget remains exhausted"
    );

    focus_section(&mut harness, "recorded.tools");
    harness.key(KeyCode::Enter);
    let calls = section_children(
        &harness.app.entity_detail.as_ref().unwrap().document,
        "recorded.tools",
    )
    .unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|node| matches!(node, DetailNode::Section { .. }))
            .count(),
        TOOL_COUNT + 1
    );
    let late = calls
        .iter()
        .find_map(|node| match node {
            DetailNode::Section { id, title, .. } if title.contains("late-budget-tool") => {
                assert!(title.contains("Exit: 7 | Duration: 125 ms"));
                Some(id.clone())
            }
            _ => None,
        })
        .expect("the late tool metadata survives retention exhaustion");
    focus_section(&mut harness, &late);
    harness.key(KeyCode::Enter);
    let (arguments, output) = tool_body_ids(&harness, &late);
    for id in [arguments, output] {
        focus_section(&mut harness, &id);
        harness.key(KeyCode::Enter);
    }
    let content = popup_text(&harness.app);
    assert!(content.contains("Call ID: late-budget-call"));
    assert_eq!(
        content
            .matches("[content not retained: 4,096-record / 2 MiB retention limit]")
            .count(),
        2
    );
    assert!(!content.contains("LATE-TOOL-ARGUMENTS-MUST-STAY-OMITTED"));
    assert!(!content.contains("LATE-TOOL-OUTPUT-MUST-STAY-OMITTED"));

    focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
    harness.key(KeyCode::Enter);
    closed_preview_teaser(&harness);
    focus_section(&mut harness, "recorded.messages");
    harness.key(KeyCode::Enter);
    let content = popup_text(&harness.app);
    assert!(content.contains("Message bodies: hidden"));
    assert!(
        !content.contains(FULL_END),
        "the reserved body is shown only in the expanded preview"
    );
}

#[test]
fn entity_detail_message_preview_handles_missing_owner_and_redacted_local_reads() {
    let message = full_preview_message();
    for case in ["missing-file", "owner-mismatch", "redacted"] {
        let directory = tempfile::tempdir().unwrap();
        let sessions = directory.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut snapshot = interaction_test_app(1, 1).snapshot;
        snapshot.codex_home = directory.path().to_owned();
        snapshot.turns[0].message_preview = Some(snapshot_preview(&message));
        let captured = snapshot.as_of;
        let thread_id = snapshot.tasks[0].thread_id.clone();
        let turn_id = snapshot.turns[0].turn_id.clone();
        if case != "missing-file" {
            let owner = if case == "owner-mismatch" {
                "different-owner"
            } else {
                &thread_id
            };
            let records = [
                serde_json::json!({"timestamp": captured - ChronoDuration::seconds(2), "type": "session_meta", "payload": {"id": owner}}).to_string(),
                serde_json::json!({"timestamp": captured - ChronoDuration::seconds(1), "type": "event_msg", "payload": {"type": "user_message", "turn_id": turn_id, "message": message}}).to_string(),
            ].join("\n");
            std::fs::write(sessions.join(format!("rollout-{thread_id}.jsonl")), records).unwrap();
        }
        let mut harness = TuiHarness::from_snapshot(snapshot, 60, 24, Theme::Light);
        harness.app.local_redact_content = case == "redacted";
        harness.app.focus_turns();
        harness.render();
        open(&mut harness);
        focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
        harness.key(KeyCode::Enter);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !harness.app.poll_entity_detail() {
            assert!(
                Instant::now() < deadline,
                "bounded {case} reader did not finish"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        harness.render();
        let content = popup_text(&harness.app);
        assert!(section_expanded(&harness, MESSAGE_PREVIEW_SECTION));
        assert!(
            content.contains("Full message unavailable"),
            "explicit {case} fallback"
        );
        assert!(!content.contains("PREVIEW-FULL-END"));
        if case == "redacted" {
            assert!(content.contains("content redacted"));
            assert!(!content.contains(&snapshot_preview(&message)));
        } else {
            assert!(content.contains(&snapshot_preview(&message)));
        }
    }
}

#[test]
fn entity_detail_reads_local_summary_sessions_that_have_aged_out_of_overview() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut harness = historical_summary_harness(80, 24, Theme::Dark);
    harness.app.snapshot.codex_home = directory.path().to_path_buf();
    harness.app.local_snapshot.tasks.clear();
    let local_id: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
    harness.app.history_local_source_id = Some(local_id.clone());
    let range = harness
        .app
        .summary_cache
        .as_ref()
        .unwrap()
        .prepared
        .usage
        .window;
    let captured = harness.app.snapshot.as_of;
    let message = |time: DateTime<Utc>, id: &str, body: &str| {
        serde_json::json!({
            "timestamp": time,
            "type": "event_msg",
            "payload": {
                "type": "item_completed", "thread_id": "historical-root", "turn_id": "historical-turn",
                "item": {"type": "UserMessage", "id": id, "content": [{"text": body}]}
            }
        }).to_string()
    };
    let body = "HISTORICAL-CONTENT-OUTSIDE-OVERVIEW-WITHIN-SUMMARY-RANGE";
    let log = [
        serde_json::json!({"timestamp": range.starts_at - ChronoDuration::seconds(2), "type": "session_meta", "payload": {"id": "historical-root"}}).to_string(),
        message(range.starts_at - ChronoDuration::seconds(1), "before", "BEFORE-SUMMARY-RANGE-MUST-NOT-APPEAR"),
        message(captured - ChronoDuration::minutes(30), "within", body),
        message(captured.max(range.ends_at) + ChronoDuration::hours(2), "future", "AFTER-CAPTURE-MUST-NOT-APPEAR"),
        message(range.ends_at, "exact-end", "EXACT-RANGE-END-MUST-NOT-APPEAR"),
    ].join("\n");
    std::fs::write(sessions.join("rollout-historical-root.jsonl"), log).unwrap();

    let rows = harness.app.summary_rows();
    let project_id = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Project)
        .unwrap()
        .id
        .clone();
    harness.app.summary_expanded_nodes.insert(project_id);
    harness.render();
    let rows = harness.app.summary_rows();
    let session_id = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Session)
        .unwrap()
        .id
        .clone();
    harness
        .app
        .summary_expanded_nodes
        .insert(session_id.clone());
    harness.render();
    let rows = harness.app.summary_rows();
    let turn_id = rows
        .iter()
        .find(|row| row.kind == SummaryRowKind::Turn && row.label == "Historical prompt")
        .unwrap()
        .id
        .clone();

    for scope in [
        HistorySourceSelection::Local(local_id),
        HistorySourceSelection::AllIncluded,
    ] {
        harness.app.history_source_applied_selection = scope.clone();
        harness.app.history_source_selection = scope;
        for selected in [&session_id, &turn_id] {
            harness.app.summary_selected_id = Some(selected.clone());
            harness.render();
            // A capture can happen after a historical report has ended; its end stays exclusive.
            harness.app.summary_cache.as_mut().unwrap().snapshot_as_of =
                range.ends_at + ChronoDuration::hours(1);
            open_expanded(&mut harness);
            assert!(
                harness.app.entity_detail_loading(),
                "local history supplies an exact owner even after Overview expires"
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            while !harness.app.poll_entity_detail() {
                assert!(
                    Instant::now() < deadline,
                    "historical detail loading did not complete"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            harness.render();
            expand_all(&mut harness);
            let content = popup_text(&harness.app);
            assert!(content.contains("Messages (1)"));
            assert!(content.contains(&format!(
                "user | phase unrecorded | {} | turn historical-turn",
                (captured - ChronoDuration::minutes(30)).to_rfc3339()
            )));
            for excluded in [
                range.starts_at - ChronoDuration::seconds(1),
                captured.max(range.ends_at) + ChronoDuration::hours(2),
                range.ends_at,
            ] {
                assert!(!content.contains(&format!(
                    "user | phase unrecorded | {} |",
                    excluded.to_rfc3339()
                )));
            }
            assert!(!content.contains(body));
            assert!(!content.contains("BEFORE-SUMMARY-RANGE-MUST-NOT-APPEAR"));
            assert!(!content.contains("AFTER-CAPTURE-MUST-NOT-APPEAR"));
            assert!(!content.contains("EXACT-RANGE-END-MUST-NOT-APPEAR"));
            harness.key(KeyCode::Esc);
        }
    }

    harness.app.history_remote_sources = vec![(
        "node-0123456789abcdef0123456789abcdef".parse().unwrap(),
        "other-machine".to_string(),
    )];
    harness.app.history_source_applied_selection = HistorySourceSelection::AllIncluded;
    harness.app.summary_selected_id = Some(turn_id);
    harness.render();
    open_expanded(&mut harness);
    assert!(!harness.app.entity_detail_loading());
    assert!(!harness.app.poll_entity_detail());
    assert!(popup_text(&harness.app).contains("no unambiguous local rollout association"));
    assert!(!popup_text(&harness.app).contains(body));
}

#[test]
fn entity_detail_restores_only_known_local_scopes_for_historical_summary_ids() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = directory.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let local_id: NodeId = "node-11111111111111111111111111111111".parse().unwrap();
    let other_id: NodeId = "node-22222222222222222222222222222222".parse().unwrap();
    let body = "SCOPED-HISTORY-READS-RAW-LOCAL-OWNER-AND-TURN";

    for (case, session_allowed, turn_allowed) in [
        ("local", true, true),
        ("other-session", false, false),
        ("other-turn", true, false),
        ("logical", false, false),
    ] {
        for (scope_name, scope, scope_allowed) in [
            (
                "Local",
                HistorySourceSelection::Local(local_id.clone()),
                true,
            ),
            ("AllIncluded", HistorySourceSelection::AllIncluded, true),
            (
                "different Local node",
                HistorySourceSelection::Local(other_id.clone()),
                false,
            ),
        ] {
            let mut harness = historical_summary_harness(80, 24, Theme::Dark);
            harness.app.snapshot.codex_home = directory.path().to_path_buf();
            harness.app.local_snapshot.tasks.clear();
            assert!(harness.app.snapshot.tasks.is_empty());
            assert!(harness.app.local_snapshot.tasks.is_empty());
            harness.app.history_local_source_id = Some(local_id.clone());
            harness.app.history_remote_sources.clear();
            harness.app.history_source_applied_selection = scope.clone();
            harness.app.history_source_selection = scope;

            // Match scope_project_groups: thread, parent and session IDs share
            // their source suffix; both exact turn IDs are scoped separately.
            let thread_source = if case == "other-session" {
                &other_id
            } else {
                &local_id
            };
            let turn_source = if case == "other-turn" {
                &other_id
            } else {
                &local_id
            };
            let scoped_thread = |raw: &str| {
                if case == "logical" && raw == "historical-root" {
                    format!("logical-thread:{raw}")
                } else {
                    format!("{raw}@{}", thread_source.as_str())
                }
            };
            let scoped_turn = |raw: &str| format!("{raw}@{}", turn_source.as_str());
            let expected_thread = scoped_thread("historical-root");
            let expected_turn = scoped_turn("historical-turn");
            let mut bucket = harness.app.history.half_hour_buckets[0].clone();
            for group in &mut bucket.project_groups {
                group.thread_id = scoped_thread(&group.thread_id);
                group.parent_thread_id = group.parent_thread_id.as_deref().map(scoped_thread);
                group.session_thread_id = group.session_thread_id.as_deref().map(scoped_thread);
                group.turn_id = group.turn_id.as_deref().map(scoped_turn);
                group.session_turn_id = group.session_turn_id.as_deref().map(scoped_turn);
            }
            harness.app.history.half_hour_buckets = vec![bucket].into();
            harness.app.summary_cache = None;
            harness.render();

            // The rollout retains raw IDs; using the Summary IDs directly
            // would miss both the owner and its exact user turn.
            let captured = harness.app.snapshot.as_of;
            let log = [
                serde_json::json!({
                    "timestamp": captured - ChronoDuration::hours(1),
                    "type": "session_meta", "payload": {"id": "historical-root"}
                }).to_string(),
                serde_json::json!({
                    "timestamp": captured - ChronoDuration::minutes(30),
                    "type": "event_msg",
                    "payload": {
                        "type": "item_completed", "thread_id": "historical-root", "turn_id": "historical-turn",
                        "item": {"type": "UserMessage", "id": "scoped-history-message", "content": [{"text": body}]}
                    }
                }).to_string(),
            ].join("\n");
            std::fs::write(sessions.join("rollout-historical-root.jsonl"), log).unwrap();

            let project = harness
                .app
                .summary_rows()
                .into_iter()
                .find(|row| row.kind == SummaryRowKind::Project)
                .unwrap();
            harness.app.summary_expanded_nodes.insert(project.id);
            harness.render();
            let session = harness
                .app
                .summary_rows()
                .into_iter()
                .find(|row| row.kind == SummaryRowKind::Session)
                .unwrap();
            harness
                .app
                .summary_expanded_nodes
                .insert(session.id.clone());
            harness.render();
            let turn = harness
                .app
                .summary_rows()
                .into_iter()
                .find(|row| row.kind == SummaryRowKind::Turn && row.label == "Historical prompt")
                .unwrap();

            for (selected, allowed, is_turn) in [
                (&session.id, session_allowed, false),
                (&turn.id, turn_allowed, true),
            ] {
                harness.app.summary_selected_id = Some(selected.clone());
                harness.render();
                open_expanded(&mut harness);
                let allowed = allowed && scope_allowed;
                assert_eq!(
                    harness.app.entity_detail_loading(),
                    allowed,
                    "{case}, {scope_name}, turn={is_turn}"
                );
                if allowed {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !harness.app.poll_entity_detail() {
                        assert!(
                            Instant::now() < deadline,
                            "scoped history loading did not complete: {case}, {scope_name}, turn={is_turn}"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    harness.render();
                    expand_all(&mut harness);
                    assert!(
                        popup_text(&harness.app).contains("Messages (1)"),
                        "{case}, {scope_name}, turn={is_turn}"
                    );
                    assert!(popup_text(&harness.app).contains(&format!(
                        "user | phase unrecorded | {} | turn historical-turn",
                        (captured - ChronoDuration::minutes(30)).to_rfc3339()
                    )));
                } else {
                    assert!(!harness.app.poll_entity_detail());
                    assert!(
                        popup_text(&harness.app)
                            .contains("no unambiguous local rollout association")
                    );
                    assert!(!popup_text(&harness.app).contains(body));
                }
                let content = popup_text(&harness.app);
                assert!(!content.contains(body));
                assert!(content.contains(&format!("Thread ID: {expected_thread}")));
                if is_turn {
                    assert!(content.contains(&format!("Turn ID: {expected_turn}")));
                }
                harness.key(KeyCode::Esc);
            }
        }
    }
}

#[test]
fn entity_detail_refresh_keeps_the_open_identity_and_frozen_values() {
    let mut harness = detail_harness(80, 24, Theme::Dark);
    harness.app.selected_task = 1;
    harness.app.selected_turn = 1;
    harness.app.focus_turns();
    harness.render();
    open(&mut harness);
    focus_section(&mut harness, MESSAGE_PREVIEW_SECTION);
    harness.key(KeyCode::Enter);
    let original = popup_text(&harness.app);
    let title = harness.app.entity_detail.as_ref().unwrap().title.clone();
    let mut snapshot = harness.app.snapshot.clone();
    snapshot.tasks.reverse();
    snapshot.turns.reverse();
    for task in &mut snapshot.tasks {
        task.title = "new snapshot title".to_string();
    }
    for turn in &mut snapshot.turns {
        turn.message_preview = Some("new snapshot message".to_string());
    }
    snapshot.as_of += ChronoDuration::seconds(30);
    harness.app.replace(
        CollectionResult {
            snapshot,
            account: harness.app.account.clone(),
            history_observation: crate::history::HistoryObservation::default(),
            local_session_digests: Default::default(),
        },
        false,
    );
    harness.render();
    assert_eq!(harness.app.entity_detail.as_ref().unwrap().title, title);
    assert_eq!(popup_text(&harness.app), original);
    assert!(original.contains("turn-1-1"));
    assert!(original.contains("message 1/1"));
    assert!(!original.contains("new snapshot message"));
    let selected = harness.app.selected_turn_record().unwrap().turn_id.clone();
    harness.key(KeyCode::Esc);
    assert_eq!(
        harness.app.selected_turn_record().unwrap().turn_id,
        selected
    );
}
