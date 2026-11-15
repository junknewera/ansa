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
        role: role.into(),
        text: text.into(),
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
            .any(|line| line.contains("\x1b[38;2;231;196;112m"))
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
    assert!(text.contains("src/retry.rs") && !text.contains("CHAT_MARKER"));
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
    assert!(selection.frame(true).lines[row].contains("\x1b[48;2;28;68;44m"));
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
