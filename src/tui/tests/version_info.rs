use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Modifier;

use super::super::*;
use super::testkit::{ClickEdge, ControlId, TuiHarness};

const SIZES: [(u16, u16); 4] = [(120, 40), (80, 24), (60, 24), (32, 14)];

fn version_harness(width: u16, height: u16, theme: Theme) -> TuiHarness {
    let mut harness = TuiHarness::from_fixture("normal", width, height, theme);
    assert!(!harness.key(KeyCode::Char('?')));
    assert!(harness.app.version_info_visible);
    harness
}

fn mouse_at(harness: &mut TuiHarness, kind: MouseEventKind, column: u16, row: u16) -> bool {
    let handled = handle_mouse_event(
        &mut harness.app,
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    );
    harness.render();
    handled
}

fn assert_only_shortcut_is_accented(harness: &TuiHarness, control: ControlId) {
    harness.assert_shortcut_distinct(control);
    let area = harness.control_rect(control);
    let accent = harness.app.theme.palette().accent;
    let mut binding_count = 0;
    for column in area.x..area.right() {
        let (symbol, foreground, modifier) = harness.cell_style(column, area.y);
        if symbol == control.binding() {
            binding_count += 1;
            assert!(modifier.contains(Modifier::BOLD));
            assert_eq!(foreground, accent);
        } else {
            assert_ne!(foreground, accent, "{control:?} accents {symbol:?}");
        }
    }
    assert_eq!(binding_count, 1, "{control:?} exact shortcut grapheme");
}

fn visible_version_scrollbar(harness: &TuiHarness) -> ScrollbarHitbox {
    let hitbox = harness.app.version_info_hitbox.unwrap();
    let scrollbar = hitbox
        .scrollbar
        .expect("long release notes need a scrollbar");
    assert_eq!(scrollbar.track.x, hitbox.content.right());
    assert_eq!(scrollbar.track.y, hitbox.content.y);
    assert_eq!(scrollbar.track.height, hitbox.content.height);
    assert_eq!(scrollbar.track.width, 1);
    assert_eq!(
        scrollbar.max_offset,
        harness
            .app
            .version_info_line_count
            .saturating_sub(usize::from(hitbox.content.height))
    );
    assert!(scrollbar.thumb.y >= scrollbar.track.y);
    assert!(scrollbar.thumb.bottom() <= scrollbar.track.bottom());
    assert!(scrollbar.thumb.height > 0);
    assert!(scrollbar.thumb.height < scrollbar.track.height);
    for row in scrollbar.track.y..scrollbar.track.bottom() {
        let on_thumb = rect_contains(scrollbar.thumb, scrollbar.track.x, row);
        let (symbol, foreground, modifier) = harness.cell_style(scrollbar.track.x, row);
        assert_eq!(symbol, if on_thumb { "█" } else { "│" });
        assert_eq!(
            foreground,
            if on_thumb {
                harness.app.theme.palette().accent
            } else {
                harness.app.theme.palette().border
            }
        );
        assert_eq!(modifier.contains(Modifier::BOLD), on_thumb);
        assert_eq!(
            harness.cell_background(scrollbar.track.x, row),
            harness.app.theme.palette().background
        );
    }
    scrollbar
}

#[test]
fn version_info_displays_current_binary_metadata_and_matching_release_notes() {
    let harness = version_harness(120, 40, Theme::Dark);
    let rendered = harness.frame().snapshot_text();
    assert!(rendered.contains(crate::version_info::VERSION));
    assert!(rendered.contains(crate::version_info::BUILD_ID));
    assert!(rendered.contains(crate::version_info::TARGET));
    let notes = crate::version_info::current_release_notes()
        .expect("the current packaged version should have bundled release notes");
    assert!(rendered.contains(notes.date));
    let first_heading = notes
        .body
        .lines()
        .find_map(|line| line.strip_prefix("### "))
        .expect("current release notes should contain a section");
    assert!(rendered.contains(first_heading));
}

#[test]
fn version_info_controls_style_exact_bindings_and_keep_geometry_in_both_themes() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = TuiHarness::from_fixture("normal", width, height, theme);
            harness.key(KeyCode::Char('4'));
            assert_only_shortcut_is_accented(&harness, ControlId::VersionInfo);
            let entry = harness.control_rect(ControlId::VersionInfo);
            assert!(harness.click(ControlId::VersionInfo, ClickEdge::End));
            harness.assert_shortcut_inactive(ControlId::VersionInfo);
            assert_eq!(harness.control_rect(ControlId::VersionInfo), entry);
            let before = harness.app.version_info_hitbox.unwrap();
            for control in [
                ControlId::VersionInfoUp,
                ControlId::VersionInfoDown,
                ControlId::VersionInfoBack,
            ] {
                assert_only_shortcut_is_accented(&harness, control);
            }
            harness.key(KeyCode::End);
            let after = harness.app.version_info_hitbox.unwrap();
            assert_eq!(before.content, after.content);
            assert_eq!(before.up, after.up);
            assert_eq!(before.down, after.down);
            assert_eq!(before.back, after.back);
            assert!(harness.click(ControlId::VersionInfoBack, ClickEdge::End));
            assert_eq!(harness.control_rect(ControlId::VersionInfo), entry);
            assert_only_shortcut_is_accented(&harness, ControlId::VersionInfo);
        }
    }
}

#[test]
fn version_info_paints_the_theme_background_in_full_and_tiny_layouts() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let harness = version_harness(width, height, theme);
            let content = harness.app.version_info_hitbox.unwrap().content;
            let popup = Rect::new(
                content.x.saturating_sub(1),
                content.y.saturating_sub(1),
                content.width.saturating_add(2),
                content.height.saturating_add(3),
            );
            for row in popup.y..popup.bottom() {
                for column in popup.x..popup.right() {
                    assert_eq!(
                        harness.cell_background(column, row),
                        theme.palette().background,
                        "{theme:?} {width}x{height} at ({column}, {row})"
                    );
                }
            }
        }
        for (width, height) in [(2, 1), (1, 1)] {
            let harness = version_harness(width, height, theme);
            for row in 0..height {
                for column in 0..width {
                    assert_eq!(
                        harness.cell_background(column, row),
                        theme.palette().background,
                        "{theme:?} tiny {width}x{height} at ({column}, {row})"
                    );
                }
            }
        }
    }
}

#[test]
fn version_info_entry_and_popup_controls_are_whole_label_clickable() {
    for (width, height) in SIZES {
        for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
            let mut harness = TuiHarness::from_fixture("normal", width, height, Theme::Dark);
            harness.key(KeyCode::Char('4'));
            assert!(harness.click(ControlId::VersionInfo, edge));
            assert!(harness.app.version_info_visible);
            assert!(harness.click(ControlId::VersionInfoDown, edge));
            assert!(harness.app.version_info_offset > 0);
            assert!(harness.click(ControlId::VersionInfoUp, edge));
            assert_eq!(harness.app.version_info_offset, 0);
            assert!(harness.click(ControlId::VersionInfoBack, edge));
            assert!(!harness.app.version_info_visible);
            assert!(!harness.app.quit_requested);
        }
    }
}

#[test]
fn version_info_entry_stays_visible_when_compact_settings_scrolls() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut harness = TuiHarness::from_fixture("normal", 32, 14, theme);
        harness.key(KeyCode::Char('4'));
        let initial_entry = harness.control_rect(ControlId::VersionInfo);
        assert!(!initial_entry.is_empty());
        harness.app.selected_setting = SettingItem::ApiEquivalent.index();
        harness.render();
        assert_eq!(harness.control_rect(ControlId::VersionInfo), initial_entry);
        assert!(
            !harness
                .control_rect(ControlId::SettingApiEquivalent)
                .is_empty()
        );
        assert_only_shortcut_is_accented(&harness, ControlId::VersionInfo);
        assert!(!harness.key(KeyCode::Char('?')));
        assert!(harness.app.version_info_visible);
        harness.key(KeyCode::Esc);
        for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
            assert!(harness.click(ControlId::VersionInfo, edge));
            assert!(harness.app.version_info_visible);
            assert_eq!(
                harness.app.selected_setting,
                SettingItem::ApiEquivalent.index()
            );
            harness.key(KeyCode::Esc);
        }
        assert_eq!(harness.control_rect(ControlId::VersionInfo), initial_entry);
    }
}

#[test]
fn version_info_entry_renders_and_opens_in_a_single_row_settings_panel() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut harness = TuiHarness::from_fixture("normal", 32, 3, theme);
        harness.key(KeyCode::Char('4'));
        let entry = harness.control_rect(ControlId::VersionInfo);
        assert!(!entry.is_empty());
        assert_only_shortcut_is_accented(&harness, ControlId::VersionInfo);
        assert!(!harness.key(KeyCode::Char('?')));
        assert!(harness.app.version_info_visible);
        harness.key(KeyCode::Esc);
        for edge in [ClickEdge::Start, ClickEdge::Middle, ClickEdge::End] {
            assert!(harness.click(ControlId::VersionInfo, edge));
            assert!(harness.app.version_info_visible);
            harness.key(KeyCode::Esc);
        }
        harness.key(KeyCode::End);
        assert_eq!(harness.control_rect(ControlId::VersionInfo), entry);
        assert_only_shortcut_is_accented(&harness, ControlId::VersionInfo);
        assert!(harness.click(ControlId::VersionInfo, ClickEdge::End));
        assert!(harness.app.version_info_visible);
    }
}

#[test]
fn version_info_global_shortcut_and_close_keys_restore_each_underlying_view() {
    for view_key in ['1', '2', 'U', '3', '4'] {
        for close_key in [
            KeyCode::Esc,
            KeyCode::Left,
            KeyCode::Char('?'),
            KeyCode::Char('q'),
        ] {
            let mut harness = TuiHarness::from_fixture("normal", 80, 24, Theme::Dark);
            harness.key(KeyCode::Char(view_key));
            let before = harness.state();
            assert!(!harness.key(KeyCode::Char('?')));
            assert!(harness.app.version_info_visible);
            assert!(!harness.key(close_key));
            assert!(!harness.app.version_info_visible);
            assert_eq!(
                harness.state(),
                before,
                "view={view_key} close={close_key:?}"
            );
            assert!(!harness.app.quit_requested);
        }
    }
}

#[test]
fn version_info_consumes_background_keyboard_and_mouse_actions() {
    let mut harness = TuiHarness::from_fixture("normal", 80, 24, Theme::Light);
    let settings = harness.control_rect(ControlId::ViewSettings);
    let before = harness.state();
    harness.key(KeyCode::Char('?'));
    for key in [
        KeyCode::Char('4'),
        KeyCode::Char('T'),
        KeyCode::Char('V'),
        KeyCode::Char('M'),
        KeyCode::Char('U'),
        KeyCode::Char('/'),
        KeyCode::Enter,
        KeyCode::Tab,
        KeyCode::Right,
    ] {
        assert!(!harness.key(key));
        assert!(harness.app.version_info_visible);
        assert_eq!(harness.state(), before, "background key={key:?}");
    }
    assert!(mouse_at(
        &mut harness,
        MouseEventKind::Down(MouseButton::Left),
        settings.x,
        settings.y,
    ));
    assert!(harness.app.version_info_visible);
    assert_eq!(harness.state(), before);
    assert!(mouse_at(
        &mut harness,
        MouseEventKind::ScrollDown,
        settings.x,
        settings.y,
    ));
    assert_eq!(harness.state(), before);
}

#[test]
fn version_info_opening_ends_background_mouse_drags_before_rendering() {
    let mut harness = TuiHarness::from_fixture("normal", 80, 24, Theme::Dark);
    harness.app.scroll_drag = Some(ScrollDrag {
        target: ScrollTarget::Tasks,
        grab_row: 0,
        pointer_row: Some(12),
    });
    harness.app.trend_drag = Some(TrendDrag {
        panel: TrendPanelId::Remaining,
    });
    harness.app.summary_daily_dragging = true;
    assert!(!handle_key_event(
        &mut harness.app,
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
    ));
    assert!(harness.app.version_info_visible);
    assert!(harness.app.scroll_drag.is_none());
    assert!(harness.app.trend_drag.is_none());
    assert!(!harness.app.summary_daily_dragging);
}

#[test]
fn version_info_shortcut_is_consumed_by_text_entry_and_existing_modals() {
    for focus in [Focus::TaskSearch, Focus::TurnSearch] {
        let mut harness = TuiHarness::from_fixture("normal", 60, 24, Theme::Dark);
        harness.app.focus = focus;
        harness.render();
        let before_view = harness.app.view;
        harness.key(KeyCode::Char('?'));
        assert!(!harness.app.version_info_visible);
        assert_eq!(harness.app.focus, focus);
        assert_eq!(harness.app.view, before_view);
        assert_eq!(
            if focus == Focus::TaskSearch {
                &harness.app.task_search
            } else {
                &harness.app.turn_search
            },
            "?"
        );
    }
    let mut harness = TuiHarness::from_fixture("normal", 80, 24, Theme::Dark);
    harness.key(KeyCode::Esc);
    assert!(harness.app.quit_confirmation_visible);
    harness.key(KeyCode::Char('?'));
    assert!(harness.app.quit_confirmation_visible);
    assert!(!harness.app.version_info_visible);

    harness.key(KeyCode::Esc);
    harness.app.remote_editor = Some(RemoteEditorState {
        mode: RemoteEditorMode::Add,
        host_id: String::new(),
        ssh_host: String::new(),
        agent_executable: DEFAULT_REMOTE_AGENT_EXECUTABLE.to_string(),
        redact_content: true,
        field: RemoteEditorField::HostId,
        host_id_cursor: 0,
        ssh_host_cursor: 0,
        agent_executable_cursor: 0,
        config_revision: 0,
        validation_error: None,
    });
    harness.render();
    harness.key(KeyCode::Char('?'));
    assert!(!harness.app.version_info_visible);
    assert_eq!(harness.app.remote_editor.as_ref().unwrap().host_id, "?");
    harness.key(KeyCode::Esc);
    harness.app.remote_update_dialog = Some(RemoteUpdateDialog {
        host_id: "dev".to_string(),
        agent_executable: DEFAULT_REMOTE_AGENT_EXECUTABLE.to_string(),
        config_revision: 0,
        scope: UiRemoteUpdateScope::Sync,
        adopt: false,
    });
    harness.render();
    harness.key(KeyCode::Char('?'));
    assert!(!harness.app.version_info_visible);
    assert!(harness.app.remote_update_dialog.is_some());
}

#[test]
fn version_info_supports_keyboard_and_mouse_scroll_with_bounds() {
    let mut harness = version_harness(60, 24, Theme::Dark);
    let viewport = usize::from(harness.app.version_info_hitbox.unwrap().content.height);
    let max_offset = harness.app.version_info_line_count.saturating_sub(viewport);
    assert!(max_offset > viewport, "release notes must exercise paging");
    harness.key(KeyCode::Down);
    assert_eq!(harness.app.version_info_offset, 1);
    harness.key(KeyCode::Up);
    assert_eq!(harness.app.version_info_offset, 0);
    harness.key(KeyCode::PageDown);
    let first_page_offset = harness.app.version_info_offset;
    assert!(first_page_offset > 1);
    harness.key(KeyCode::PageUp);
    assert_eq!(harness.app.version_info_offset, 0);
    harness.key(KeyCode::End);
    assert_eq!(harness.app.version_info_offset, max_offset);
    let last_page = harness.frame();
    harness.key(KeyCode::Down);
    harness.key(KeyCode::PageDown);
    assert_eq!(harness.app.version_info_offset, max_offset);
    assert_eq!(harness.frame(), last_page);
    harness.key(KeyCode::Home);
    assert_eq!(harness.app.version_info_offset, 0);
    let content = harness.app.version_info_hitbox.unwrap().content;
    assert!(mouse_at(
        &mut harness,
        MouseEventKind::ScrollDown,
        content.x,
        content.y,
    ));
    assert!(harness.app.version_info_offset > 0);
    assert!(mouse_at(
        &mut harness,
        MouseEventKind::ScrollUp,
        content.x,
        content.y,
    ));
    assert_eq!(harness.app.version_info_offset, 0);
    harness.key(KeyCode::Up);
    harness.key(KeyCode::PageUp);
    assert_eq!(harness.app.version_info_offset, 0);
}

#[test]
fn version_info_resize_clamps_scroll_and_extremely_small_terminals_are_safe() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut harness = version_harness(32, 14, theme);
        harness.key(KeyCode::End);
        let compact_offset = harness.app.version_info_offset;
        harness.resize(120, 40);
        let content = harness.app.version_info_hitbox.unwrap().content;
        let max_offset = harness
            .app
            .version_info_line_count
            .saturating_sub(usize::from(content.height));
        assert!(compact_offset > max_offset);
        assert_eq!(harness.app.version_info_offset, max_offset);
        for (width, height) in [(20, 8), (8, 4), (2, 1), (1, 1), (0, 0)] {
            harness.resize(width, height);
            harness.key(KeyCode::Down);
            harness.key(KeyCode::PageDown);
            harness.key(KeyCode::End);
            assert!(harness.app.version_info_visible);
        }
        harness.resize(60, 24);
        assert_only_shortcut_is_accented(&harness, ControlId::VersionInfoBack);
        assert!(!harness.key(KeyCode::Esc));
        assert!(!harness.app.version_info_visible);
    }
}

#[test]
fn version_info_scrollbar_tracks_keyboard_and_wheel_scrolling_in_both_themes() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in SIZES {
            let mut harness = version_harness(width, height, theme);
            let initial = visible_version_scrollbar(&harness);
            assert_eq!(initial.thumb.y, initial.track.y);
            harness.key(KeyCode::Down);
            assert_eq!(harness.app.version_info_offset, 1);
            assert_eq!(visible_version_scrollbar(&harness).track, initial.track);
            harness.key(KeyCode::Up);
            assert_eq!(harness.app.version_info_offset, 0);
            assert_eq!(visible_version_scrollbar(&harness).thumb.y, initial.track.y);
            harness.key(KeyCode::PageDown);
            let page_offset = harness.app.version_info_offset;
            assert!(page_offset > 1);
            let bar = visible_version_scrollbar(&harness);
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::ScrollDown,
                bar.track.x,
                bar.track.y,
            ));
            let wheel_offset = harness.app.version_info_offset;
            assert!(wheel_offset > page_offset);
            let bar = visible_version_scrollbar(&harness);
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::ScrollUp,
                bar.track.x,
                bar.track.y,
            ));
            assert_eq!(harness.app.version_info_offset, page_offset);
            harness.key(KeyCode::End);
            let end = visible_version_scrollbar(&harness);
            assert_eq!(harness.app.version_info_offset, end.max_offset);
            assert_eq!(end.thumb.bottom(), end.track.bottom());
            assert_eq!(end.track, initial.track);
            harness.key(KeyCode::Down);
            assert_eq!(visible_version_scrollbar(&harness), end);
            harness.key(KeyCode::Home);
            assert_eq!(harness.app.version_info_offset, 0);
            assert_eq!(visible_version_scrollbar(&harness).thumb.y, initial.track.y);
        }
    }
}

#[test]
fn version_info_scrollbar_resizes_and_disappears_in_empty_viewports() {
    for theme in [Theme::Dark, Theme::Light] {
        let mut harness = version_harness(32, 14, theme);
        harness.key(KeyCode::End);
        let compact = visible_version_scrollbar(&harness);
        harness.resize(120, 40);
        let wide = visible_version_scrollbar(&harness);
        assert!(wide.max_offset < compact.max_offset);
        assert_eq!(harness.app.version_info_offset, wide.max_offset);
        assert_eq!(wide.thumb.bottom(), wide.track.bottom());
        harness.resize(32, 14);
        let compact = visible_version_scrollbar(&harness);
        assert!(compact.max_offset > wide.max_offset);
        assert_eq!(harness.app.version_info_offset, wide.max_offset);
        assert!(compact.thumb.bottom() < compact.track.bottom());
        for (width, height) in [(8, 4), (2, 1), (1, 1), (0, 0)] {
            harness.resize(width, height);
            assert!(harness.app.version_info_hitbox.unwrap().scrollbar.is_none());
            assert!(harness.app.scroll_drag.is_none());
        }
        harness.resize(60, 24);
        let restored = visible_version_scrollbar(&harness);
        assert_eq!(harness.app.version_info_offset, 0);
        assert_eq!(restored.thumb.y, restored.track.y);
    }
}

#[test]
fn version_info_scrollbar_track_and_drag_are_bounded_and_isolated() {
    for theme in [Theme::Dark, Theme::Light] {
        for (width, height) in [(60, 24), (32, 14)] {
            let mut harness = version_harness(width, height, theme);
            let underlying = harness.state();
            for _ in 0..3 {
                harness.key(KeyCode::Down);
            }
            let before_horizontal_drag = harness.app.version_info_offset;
            let bar = visible_version_scrollbar(&harness);
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Down(MouseButton::Left),
                bar.thumb.x,
                bar.thumb.y,
            ));
            assert_eq!(
                harness.app.scroll_drag.map(|drag| drag.target),
                Some(ScrollTarget::VersionInfo)
            );
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Drag(MouseButton::Left),
                0,
                bar.thumb.y,
            ));
            assert_eq!(harness.app.version_info_offset, before_horizontal_drag);
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Up(MouseButton::Left),
                0,
                bar.thumb.y,
            ));
            assert!(harness.app.scroll_drag.is_none());
            let bar = visible_version_scrollbar(&harness);
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Down(MouseButton::Left),
                bar.track.x,
                bar.track.bottom().saturating_sub(1),
            ));
            assert_eq!(harness.app.version_info_offset, bar.max_offset);
            assert_eq!(
                harness.app.scroll_drag.map(|drag| drag.target),
                Some(ScrollTarget::VersionInfo)
            );
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Drag(MouseButton::Left),
                0,
                0,
            ));
            assert_eq!(harness.app.version_info_offset, 0);
            let top = visible_version_scrollbar(&harness);
            assert_eq!(top.thumb.y, top.track.y);
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Drag(MouseButton::Left),
                u16::MAX,
                u16::MAX,
            ));
            let bottom = visible_version_scrollbar(&harness);
            assert_eq!(harness.app.version_info_offset, bottom.max_offset);
            assert_eq!(bottom.thumb.bottom(), bottom.track.bottom());
            assert_eq!(harness.state(), underlying);
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Up(MouseButton::Left),
                u16::MAX,
                u16::MAX,
            ));
            assert!(harness.app.scroll_drag.is_none());
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Drag(MouseButton::Left),
                0,
                0,
            ));
            assert_eq!(harness.app.version_info_offset, bottom.max_offset);
            assert_eq!(harness.state(), underlying);
            assert!(mouse_at(
                &mut harness,
                MouseEventKind::Down(MouseButton::Left),
                bottom.thumb.x,
                bottom.thumb.y,
            ));
            harness.key(KeyCode::Esc);
            assert!(!harness.app.version_info_visible);
            assert!(harness.app.scroll_drag.is_none());
            assert_eq!(harness.state(), underlying);
        }
    }
}
