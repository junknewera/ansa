use ansa::frontend::{
    carbon::{self, Choice, Entry, Focus, Inspector, InspectorKind, Popup, Selection, Workspace},
    input::{Action, Editor, MouseEvent, MouseKind},
};
fn feed(editor: &mut Editor, bytes: &[u8]) -> Vec<Action> {
    bytes
        .iter()
        .filter_map(|&byte| editor.feed(byte, true))
        .collect()
}
fn entry(role: &str, text: &str) -> Entry {
    Entry {
        action: None,
        edit: None,
        output: None,
        status: None,
        role: role.into(),
        text: text.into(),
    }
}
#[test]
fn full_diff_horizontal_scroll_exposes_and_copies_unicode_tails() {
    use ansa::frontend::{diff, theme::Theme};
    let before = format!("{}TAIL_界😀\n", "x".repeat(80));
    let after = format!("{}NEW_界🌍\n", "x".repeat(80));
    let rows = std::sync::Arc::new(diff::between(&before, &after).unwrap().rows);
    for size in [(42, 24), (64, 30)] {
        let render = |column| {
            let mut frame = carbon::layout_workspace(
                &[],
                b"draft",
                3,
                "ready",
                0,
                "project",
                size,
                true,
                &Workspace {
                    inspector: Some(Inspector {
                        title: "Diff".into(),
                        kind: InspectorKind::Diff,
                        diff: Some(rows.clone()),
                        diff_column: column,
                        ..Inspector::default()
                    }),
                    ..Workspace::default()
                },
            )
            .unwrap();
            frame.apply_theme(&Theme::default());
            frame
        };
        assert!(!render(0).plain_lines().join("\n").contains("TAIL_界😀"));
        let frame = render(80);
        let row = frame
            .plain_lines()
            .iter()
            .position(|line| line.contains("TAIL_界😀"))
            .unwrap();
        assert!(frame.plain_lines().join("\n").contains("NEW_界🌍"));
        let mut selection = Selection::new(frame, 12, row);
        assert_eq!(selection.finish(20, row).as_deref(), Some("TAIL_界😀"));
    }
}

#[test]
fn full_diff_clips_wide_glyphs_at_the_right_panel_edge() {
    use ansa::frontend::{diff, theme::Theme};
    let numbered = diff::Row {
        kind: diff::Kind::Context,
        old: Some(15000),
        new: Some(15000),
        text: "x".repeat(40),
    };
    assert_eq!(numbered.horizontal_limit(34), 20);
    let notice = diff::Row {
        kind: diff::Kind::Notice,
        old: None,
        new: None,
        text: "x".repeat(80),
    };
    assert_eq!(notice.horizontal_limit(34), 0);
    for glyph in ["界", "😀", "👩‍💻"] {
        let before = format!("{}{}tail\n", "x".repeat(21), glyph);
        let rows = std::sync::Arc::new(diff::between(&before, "new\n").unwrap().rows);
        for column in 0..26 {
            let mut frame = carbon::layout_workspace(
                &[],
                b"",
                0,
                "",
                0,
                "project",
                (35, 24),
                true,
                &Workspace {
                    inspector: Some(Inspector {
                        title: "Diff".into(),
                        kind: InspectorKind::Diff,
                        diff: Some(rows.clone()),
                        diff_column: column,
                        ..Inspector::default()
                    }),
                    ..Workspace::default()
                },
            )
            .unwrap();
            frame.apply_theme(&Theme::default());
            for line in frame.plain_lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line.as_str()) <= 34,
                    "column={column}, glyph={glyph}, line={line:?}"
                );
            }
        }
    }
}

#[test]
fn selection_and_resize_preserve_unselected_colors_and_backgrounds() {
    let frame = carbon::layout_workspace(
        &[
            entry("user", "USER COLOR"),
            entry("assistant", "# HEAD\nColor `inline` and Ж界😀 end"),
        ],
        b"draft",
        3,
        "ready",
        0,
        "project",
        (60, 24),
        true,
        &Workspace::default(),
    )
    .unwrap();
    let row = frame
        .plain_lines()
        .iter()
        .position(|line| line.contains("Color inline"))
        .unwrap();
    let mut selection = Selection::new(frame.clone(), 0, row);
    selection.drag(4, row);
    let selected = selection.frame_at((60, 24), true);
    for (index, line) in frame.lines.iter().enumerate() {
        if index != row {
            assert_eq!(&selected.lines[index], line, "unselected row lost styling");
        }
    }
    assert!(
        selected.lines[row].contains("\x1b[38;2;229;193;137m"),
        "inline code after the selection lost its color"
    );
    let narrow = selection.frame_at((24, 24), true);
    assert!(
        narrow
            .lines
            .iter()
            .any(|line| line.contains("\x1b[38;2;188;166;242m")),
        "clipping dropped user accent"
    );
    assert!(
        !frame.lines.join("\n").contains("\x1b[48;2;"),
        "chat must retain terminal background"
    );
    assert_eq!(selection.finish(4, row).as_deref(), Some("Color"));
    assert!(!selection.text().unwrap().contains('\x1b'));
}

#[test]
fn semantic_palette_keeps_statuses_distinct_and_plain_output_unstyled() {
    let mut history = vec![
        entry("user", "Прочитай README Ж界😀"),
        entry("assistant", "# Обзор\nОтвет с `кодом`.\x1b[31m"),
    ];
    for (role, text) in [
        ("tool_success", "> Чтение README.md · готово · 20 мс"),
        ("tool_running", "> Запуск cargo test · выполняется"),
        ("tool_error", "> Запуск false · ошибка"),
        (
            "tool_warning",
            "> Запуск command · результат неизвестен (effect_unknown)",
        ),
        ("tool_detail", "  Вывод:\n    CONTENT Ж界😀"),
    ] {
        let mut row = entry(role, text);
        if role != "tool_detail" {
            row.action = Some(role.into());
        }
        history.push(row);
    }
    for size in [(100, 32), (42, 32)] {
        let render = |color| {
            carbon::layout_workspace(
                &history,
                b"draft",
                5,
                "basic  инструмент",
                0,
                "~/ansa",
                size,
                color,
                &Workspace::default(),
            )
            .unwrap()
        };
        let colored = render(true);
        let plain = render(false);
        let raw = colored.lines.join("\n");
        let colors: std::collections::BTreeSet<_> = raw
            .split("\x1b[38;2;")
            .skip(1)
            .filter_map(|part| part.split_once('m').map(|(value, _)| value))
            .collect();
        assert!(
            colors.len() >= 6,
            "roles collapsed to one accent: {colors:?}"
        );
        let row = |needle: &str| {
            colored
                .lines
                .iter()
                .find(|line| line.contains(needle))
                .unwrap()
        };
        assert_ne!(
            row("готово").split("Чтение").next(),
            row("ошибка").split("Запуск").next()
        );
        assert!(raw.contains("▸") && raw.contains("│"));
        assert!(!raw.contains("\x1b[48;2;"));
        assert!(
            !raw.contains("\x1b[31m"),
            "untrusted controls became styles"
        );
        assert!(!plain.lines.join("\n").contains('\x1b'));
        assert_eq!(colored.cursor, plain.cursor);
        assert_eq!(colored.scroll_limit, plain.scroll_limit);
        assert!(
            colored
                .plain_lines()
                .iter()
                .any(|line| line.contains("CONTENT Ж界😀"))
        );
    }
}

#[test]
fn every_theme_keeps_the_terminal_background_and_local_selection() {
    use ansa::frontend::theme::Theme;
    for id in ["carbon", "pink", "red", "blue", "amber"] {
        let mut frame = carbon::layout_workspace(
            &[entry("user", "USER"), entry("assistant", "THEMED text")],
            b"draft",
            2,
            "",
            0,
            "project",
            (72, 24),
            true,
            &Workspace::default(),
        )
        .unwrap();
        frame.apply_theme(&Theme::builtin(id).unwrap());
        let raw = frame.lines.join("\n");
        assert!(!raw.contains("\x1b[48;"), "{id} painted a background");
        assert!(raw.contains("\x1b[49m"));
        let row = frame
            .plain_lines()
            .iter()
            .position(|line| line.starts_with("THEMED"))
            .unwrap();
        let mut selection = Selection::new(frame, 0, row);
        selection.drag(3, row);
        assert!(selection.frame(true).lines[row].contains("\x1b[48;"));
        assert_eq!(selection.text().as_deref(), Some("THEM"));
    }
}

#[test]
fn terminal_grid_is_clipped_sanitized_and_keeps_child_sgr_after_theme() {
    use ansa::frontend::theme::Theme;
    let grid = std::sync::Arc::new(vec![
        "\x1b[38;2;125;183;245mCHILD\x1b[48;2;1;2;3m 界😀\x1b[0m\x1b[2J\x1b]52;unsafe".into(),
    ]);
    for color in [false, true] {
        let ui = Workspace {
            focus: Focus::Inspector,
            inspector: Some(Inspector {
                title: "Терминал".into(),
                kind: InspectorKind::Command,
                rendered: Some(grid.clone()),
                cursor: Some((2, 0)),
                ..Inspector::default()
            }),
            ..Workspace::default()
        };
        let mut frame = carbon::layout_workspace(
            &[entry("assistant", "CHAT")],
            b"draft",
            2,
            "",
            0,
            "project",
            (120, 24),
            color,
            &ui,
        )
        .unwrap();
        let (column, row, width, height) = frame.inspector_bounds().unwrap();
        assert!(width > 0 && height > 0);
        assert_eq!(frame.cursor, column + 2);
        if color {
            frame.apply_theme(&Theme::builtin("pink").unwrap());
        }
        let output = &frame.lines[row];
        assert!(frame.plain_lines().join("\n").contains("CHAT"));
        assert!(
            frame
                .plain_lines()
                .iter()
                .any(|line| line.contains("Ctrl+C"))
        );
        assert!(
            !frame
                .plain_lines()
                .iter()
                .any(|line| line.contains("Tab фокус"))
        );
        assert!(!output.contains("\x1b[2J") && !output.contains("\x1b]52"));
        if color {
            assert!(
                output.contains("\x1b[38;2;125;183;245m") && output.contains("\x1b[48;2;1;2;3m")
            );
        } else {
            assert!(!frame.lines.join("\n").contains('\x1b'));
        }
        assert!(unicode_width::UnicodeWidthStr::width(frame.plain_lines()[row].as_str()) <= 119);
    }
}

#[test]
fn slash_suggestions_stay_above_composer_and_leave_right_panel_unchanged() {
    let history = (0..30)
        .map(|i| entry("assistant", &format!("CHAT {i}")))
        .collect::<Vec<_>>();
    let mut ui = workspace();
    let before =
        carbon::layout_workspace(&history, b"/mo", 3, "", 0, "project", (120, 24), false, &ui)
            .unwrap();
    ui.suggestions = Some(Popup {
        items: vec![
            Choice {
                label: "/model".into(),
                detail: "Выбрать модель".into(),
            },
            Choice {
                label: "/mode".into(),
                detail: "Режим работы".into(),
            },
        ],
        footer: "Enter/→ вставить".into(),
        ..Popup::default()
    });
    let after =
        carbon::layout_workspace(&history, b"/mo", 3, "", 0, "project", (120, 24), false, &ui)
            .unwrap();
    let lines = after.plain_lines();
    let menu = lines
        .iter()
        .position(|line| line.contains("/model"))
        .unwrap();
    let composer = lines
        .iter()
        .position(|line| line.starts_with("› /mo"))
        .unwrap();
    assert!(menu < composer);
    assert!(!lines[menu].contains("ANSA"));
    for (before, after) in before.plain_lines().iter().zip(&lines) {
        if let Some((_, right)) = before.split_once(" │ ") {
            assert_eq!(after.split_once(" │ ").unwrap().1, right);
        }
    }
}

#[test]
fn queued_user_and_activity_keep_width_and_scroll_stable_across_ticks() {
    let history = [
        entry("queued_user", "Уточнение без суффикса"),
        entry("activity", "Читаю файлы · 02:14"),
    ];
    let mut ui = Workspace {
        animate: true,
        ..Workspace::default()
    };
    let first =
        carbon::layout_workspace(&history, b"draft", 2, "", 0, "project", (72, 24), true, &ui)
            .unwrap();
    ui.animation_frame = 4;
    let next =
        carbon::layout_workspace(&history, b"draft", 2, "", 0, "project", (72, 24), true, &ui)
            .unwrap();
    assert_eq!(first.scroll_limit, next.scroll_limit);
    assert_eq!(first.cursor, next.cursor);
    assert_eq!(
        first
            .plain_lines()
            .iter()
            .map(|line| unicode_width::UnicodeWidthStr::width(line.as_str()))
            .collect::<Vec<_>>(),
        next.plain_lines()
            .iter()
            .map(|line| unicode_width::UnicodeWidthStr::width(line.as_str()))
            .collect::<Vec<_>>()
    );
    let text = next.plain_lines().join("\n");
    assert!(text.contains("◷ Уточнение без суффикса") && text.contains("Читаю файлы · 02:14"));
    assert!(!next.plain_lines()[0].contains("02:14"));
}

#[test]
fn selection_keeps_copy_bytes_while_runtime_diagnostics_update() {
    let before = carbon::layout_workspace(
        &[entry("assistant", "ORIGINAL_COPY")],
        b"draft",
        5,
        "готов",
        0,
        "",
        (80, 20),
        false,
        &Workspace::default(),
    )
    .unwrap();
    let row = before
        .plain_lines()
        .iter()
        .position(|line| line.contains("ORIGINAL_COPY"))
        .unwrap();
    let mut selection = Selection::new(before, 0, row);
    selection.drag(12, row);
    let live = carbon::layout_workspace(
        &[entry("assistant", "NEW_CONTENT")],
        b"draft",
        5,
        "ошибка сохранения\neffect_unknown: проверьте результат",
        0,
        "",
        (80, 20),
        false,
        &Workspace::default(),
    )
    .unwrap();
    let mut frame = selection.frame_at((80, 20), false);
    frame.update_status_from(&live);
    let lines = frame.plain_lines().join("\n");
    assert!(lines.contains("ORIGINAL_COPY") && !lines.contains("NEW_CONTENT"));
    assert!(lines.contains("ошибка сохранения") && lines.contains("effect_unknown"));
    assert_eq!(selection.finish(12, row).as_deref(), Some("ORIGINAL_COPY"));
    for size in [(10, 12), (80, 7)] {
        let before = carbon::layout_workspace(
            &[entry("assistant", "copy")],
            b"draft",
            5,
            "old",
            0,
            "",
            size,
            false,
            &Workspace::default(),
        )
        .unwrap();
        let row = before
            .plain_lines()
            .iter()
            .position(|line| line == "copy")
            .unwrap();
        let mut selection = Selection::new(before, 0, row);
        selection.drag(3, row);
        let live = carbon::layout_workspace(
            &[entry("assistant", "next")],
            b"draft",
            5,
            "STATE\nALERT\nHINT",
            0,
            "",
            size,
            false,
            &Workspace::default(),
        )
        .unwrap();
        let mut frame = selection.frame_at(size, false);
        frame.update_status_from(&live);
        let lines = frame.plain_lines().join("\n");
        assert!(
            lines.contains("ALERT") && lines.contains("HINT"),
            "{size:?}: {lines}"
        );
        assert_eq!(selection.finish(3, row).as_deref(), Some("copy"));
    }
}
#[test]
fn bracketed_paste_keeps_multiline_content_out_of_action_dispatch() {
    let mut editor = Editor::default();
    assert!(feed(&mut editor, b"\x1b[200~first\r\n/exit\n\x03last\x1b[201~").is_empty());
    assert!(!editor.pasting());
    assert_eq!(editor.text().unwrap(), "first\n/exit\nlast");
    assert_eq!(editor.feed(b'\r', true), Some(Action::Submit));
    editor.clear();
    let paste = [
        b"\x1b[200~".as_slice(),
        vec![b'x'; 16385].as_slice(),
        b"\x1b[201~".as_slice(),
    ]
    .concat();
    assert!(feed(&mut editor, &paste).is_empty());
    assert!(editor.overflow);
    assert_eq!(editor.bytes.len(), 16384);
    assert_eq!(editor.feed(127, true), None);
    assert_eq!(editor.feed(b'\r', true), Some(Action::Overflow));
}
#[test]
fn escape_disambiguation_mouse_and_focus_work_with_fragmented_sequences() {
    let mut editor = Editor::default();
    assert_eq!(editor.feed(27, true), None);
    assert!(editor.escape_pending());
    assert_eq!(editor.flush_escape(), Some(Action::Escape));
    assert_eq!(editor.flush_escape(), None);
    assert_eq!(feed(&mut editor, b"\x1b[A"), [Action::HistoryOlder]);
    assert_eq!(feed(&mut editor, b"\x1b[Z"), [Action::FocusPrevious]);
    assert_eq!(editor.feed(9, true), Some(Action::FocusNext));
    assert_eq!(editor.feed(6, true), None);
    assert_eq!(
        feed(&mut editor, b"\x1b[<0;4;7M"),
        [Action::Mouse(MouseEvent {
            column: 3,
            row: 6,
            kind: MouseKind::Press
        })]
    );
    assert_eq!(
        feed(&mut editor, b"\x1b[<32;9;7M"),
        [Action::Mouse(MouseEvent {
            column: 8,
            row: 6,
            kind: MouseKind::Drag
        })]
    );
    assert_eq!(
        feed(&mut editor, b"\x1b[<0;9;7m"),
        [Action::Mouse(MouseEvent {
            column: 8,
            row: 6,
            kind: MouseKind::Release
        })]
    );
    for bad in [
        b"\x1b[<0;0;1M".as_slice(),
        b"\x1b[<0;1;0M",
        b"\x1b[<1;1;1M",
        b"\x1b[<0;99999999;1M",
    ] {
        assert!(feed(&mut editor, bad).is_empty());
    }
    assert!(editor.bytes.is_empty());
}
#[test]
fn editor_multiline_and_word_edits_preserve_utf8_boundaries() {
    let mut editor = Editor::default();
    feed(&mut editor, "привет мир".as_bytes());
    editor.feed(23, true);
    assert_eq!(editor.text().unwrap(), "привет ");
    assert!(feed(&mut editor, b"\x1b[13;2u").is_empty());
    feed(&mut editor, b"second");
    editor.feed(1, true);
    feed(&mut editor, b"first ");
    assert_eq!(editor.text().unwrap(), "привет \nfirst second");
    editor.feed(11, true);
    assert_eq!(editor.text().unwrap(), "привет \nfirst ");
    assert_eq!(feed(&mut editor, b"\x1b\r"), [Action::QueueSubmit]);
    assert_eq!(editor.feed(b'\n', true), None);
    assert!(editor.text().unwrap().ends_with('\n'));
    assert_eq!(editor.feed(b'\r', true), Some(Action::Submit));
}
fn workspace() -> Workspace {
    Workspace {
        model: "fixture-model".into(),
        effort: "provider default".into(),
        activity: "LIVE · 02:14".into(),
        breadcrumb: "main / reviewer".into(),
        inspector: Some(Inspector {
            title: "src/retry.rs · READ ONLY".into(),
            tabs: vec!["Source".into(), "Diff".into(), "Preview".into()],
            lines: vec![
                "pub async fn retry() {".into(),
                "    return Ok(());".into(),
                "}".into(),
            ],
            footer: "e $EDITOR · Esc return".into(),
            kind: InspectorKind::Source,
            ..Inspector::default()
        }),
        focus: Focus::Inspector,
        ..Workspace::default()
    }
}
#[test]
fn workspace_shows_real_data_and_adapts_inspector_to_narrow_screen() {
    let history = [entry("user", "task"), entry("assistant", "CHAT_MARKER")];
    let wide = carbon::layout_workspace(
        &history,
        b"draft",
        3,
        "working",
        0,
        "~/project",
        (120, 32),
        true,
        &workspace(),
    )
    .unwrap();
    let text = wide.plain_lines().join("\n");
    assert!(text.contains("ANSA") && text.contains("CHAT_MARKER") && text.contains("src/retry.rs"));
    assert!(text.contains("Source") && text.contains("Diff") && text.contains("fixture-model"));
    assert!(!text.contains("context 18%") && !text.contains("Ctrl+F") && !text.contains("PgUp"));
    assert_eq!(wide.lines.len(), 32);
    assert!(
        wide.lines
            .iter()
            .any(|line| line.contains("\x1b[38;2;229;193;137m"))
    );
    let narrow = carbon::layout_workspace(
        &history,
        b"draft",
        3,
        "working",
        0,
        "~/project",
        (60, 24),
        false,
        &workspace(),
    )
    .unwrap();
    let text = narrow.lines.join("\n");
    assert!(text.contains("src/retry.rs") && text.contains("CHAT_MARKER"));
    assert!(!text.contains('\x1b'));
    assert_eq!(narrow.lines.len(), 24);
}
#[test]
fn model_popup_sanitizes_untrusted_fields_and_keeps_next_turn_scope_visible() {
    let mut ui = workspace();
    ui.popup = Some(Popup {
        title: "/model".into(),
        subtitle: "current agent · next turn".into(),
        items: vec![Choice {
            label: "model\x1b]52;unsafe".into(),
            detail: "current".into(),
        }],
        selected: 0,
        footer: "Enter apply · Esc cancel".into(),
    });
    let frame =
        carbon::layout_workspace(&[], b"", 0, "", 0, "project", (80, 24), false, &ui).unwrap();
    let text = frame.lines.join("\n");
    assert!(text.contains("/model") && text.contains("next turn") && text.contains("Esc cancel"));
    assert!(!text.contains('\x1b'));
}
#[test]
fn selection_copies_the_frozen_visible_text_with_utf8_and_without_sgr() {
    let first = carbon::layout_workspace(
        &[entry("assistant", "Ae\u{301}😀B")],
        b"",
        0,
        "",
        0,
        "project",
        (60, 24),
        true,
        &Workspace::default(),
    )
    .unwrap();
    let row = first
        .plain_lines()
        .iter()
        .position(|line| line.contains("Ae\u{301}😀B"))
        .unwrap();
    let mut selection = Selection::new(first, 0, row);
    assert_eq!(selection.text(), None);
    selection.drag(5, row);
    // Building a new streaming frame cannot modify the owned selection.
    let _later = carbon::layout_workspace(
        &[entry("assistant", "replaced")],
        b"",
        0,
        "",
        0,
        "project",
        (60, 24),
        true,
        &Workspace::default(),
    )
    .unwrap();
    let selected = selection.finish(5, row).unwrap();
    assert_eq!(selected, "Ae\u{301}😀B");
    assert!(!selected.contains('\x1b'));
    assert!(selection.frame(true).lines[row].contains("\x1b[48;2;46;63;88m"));
}
#[test]
fn multiline_input_retains_cursor_row_and_status_ticks_leave_chat_untouched() {
    let history = [entry("assistant", "unchanged transcript")];
    let mut ui = Workspace::default();
    let first = carbon::layout_workspace(
        &history,
        b"first\nsecond",
        8,
        "waiting",
        0,
        "project",
        (80, 24),
        false,
        &ui,
    )
    .unwrap();
    assert!(first.lines.iter().any(|row| row == "› first"));
    assert!(first.lines.iter().any(|row| row == "  second"));
    ui.activity = "LIVE · 00:02".into();
    let next = carbon::layout_workspace(
        &history,
        b"first\nsecond",
        8,
        "waiting",
        0,
        "project",
        (80, 24),
        false,
        &ui,
    )
    .unwrap();
    let delta = next.paint_changes(&first);
    assert!(
        !delta.contains("unchanged transcript")
            && !delta.contains("second")
            && !delta.contains("\x1b[2J")
    );
}
#[test]
fn tiny_workspace_keeps_multiline_draft_in_physical_cells() {
    let input = "first\nЖ界😀\nlast".as_bytes();
    let saved = input.to_vec();
    let frame = carbon::layout_workspace(
        &[],
        input,
        input.len(),
        "ready",
        0,
        "project",
        (10, 7),
        false,
        &Workspace::default(),
    )
    .unwrap();
    assert_eq!(frame.lines.len(), 7);
    assert!(
        frame
            .lines
            .iter()
            .all(|row| !row.contains('\n') && !row.contains('\r'))
    );
    assert!(frame.cursor < 10);
    for row in frame.plain_lines() {
        let width = ansa_linux::terminal::Width::default();
        assert!(
            row.chars()
                .map(|cp| width.width(cp).unwrap_or(1))
                .sum::<usize>()
                < 10
        );
    }
    assert_eq!(input, saved);
}

#[test]
fn help_uses_room_for_complete_command_descriptions() {
    let ui = Workspace {
        popup: Some(ansa::frontend::commands::help(0)),
        ..Workspace::default()
    };
    let frame =
        carbon::layout_workspace(&[], b"", 0, "", 0, "project", (100, 32), false, &ui).unwrap();
    let text = frame.plain_lines().join("\n");
    assert!(text.contains("Выбрать модель и уровень рассуждения"));
    assert!(text.contains("Файлы, diff и терминал проекта"));
    assert!(
        frame
            .plain_lines()
            .iter()
            .all(|line| ansa::frontend::cells::width(line) <= 100)
    );
}
