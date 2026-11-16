use ansa::frontend::carbon::{self, Entry, Inspector, InspectorKind, Selection, Workspace};
use ansa::frontend::cells;

fn frame(line: &str) -> carbon::Frame {
    let mut frame = carbon::layout_workspace(
        &[],
        b"draft",
        3,
        "",
        0,
        "project",
        (120, 24),
        false,
        &Workspace::default(),
    )
    .unwrap();
    frame.lines[4] = line.into();
    frame
}

// Interpret every cursor move and erase in the emitted delta. Recording writes
// as well as erasures catches clear/repaint flicker even if the final text agrees.
fn touched(delta: &str, previous: &carbon::Frame) -> Vec<(usize, usize)> {
    let mut row = previous
        .plain_lines()
        .iter()
        .position(|line| line.starts_with("› "))
        .unwrap();
    let mut column = previous.cursor;
    let mut result = Vec::new();
    let mut at = 0;
    while at < delta.len() {
        let remaining = &delta[at..];
        if let Some(csi) = remaining.strip_prefix("\x1b[") {
            let end = csi.find(|cp: char| ('@'..='~').contains(&cp)).unwrap();
            let parameters = &csi[..end];
            let command = csi.as_bytes()[end];
            let n = parameters.parse::<usize>().unwrap_or(0);
            match command {
                b'A' => row -= n.max(1),
                b'B' => row += n.max(1),
                b'C' => column += n.max(1),
                b'K' => {
                    let start = if n == 2 { 0 } else { column };
                    result.extend((start..120).map(|column| (row, column)));
                }
                b'm' => {}
                _ => panic!("unexpected delta CSI: {parameters}{}", command as char),
            }
            at += end + 3;
            continue;
        }
        let printable = remaining
            .find(['\x1b', '\r', '\n'])
            .unwrap_or(remaining.len());
        if printable > 0 {
            for cluster in cells::graphemes(&remaining[..printable]) {
                let width = cells::width(cluster);
                if width == 0 {
                    result.push((row, column.saturating_sub(1)));
                } else {
                    result.extend((column..column + width).map(|column| (row, column)));
                    column += width;
                }
            }
            at += printable;
            continue;
        }
        let cp = remaining.chars().next().unwrap();
        match cp {
            '\r' => column = 0,
            '\n' => row += 1,
            _ => panic!("unexpected delta control: {cp:?}"),
        }
        at += cp.len_utf8();
    }
    let expected_row = previous
        .plain_lines()
        .iter()
        .position(|line| line.starts_with("› "))
        .unwrap();
    assert_eq!((row, column), (expected_row, previous.cursor));
    result
}

#[test]
fn scrolling_the_split_inspector_never_erases_or_rewrites_stable_chat_cells() {
    for color in [false, true] {
        let mut ui = Workspace {
            inspector: Some(Inspector {
                title: "src/file.rs".into(),
                kind: InspectorKind::Source,
                lines: (0..60)
                    .map(|i| format!("    let value_{i} = {i};"))
                    .collect(),
                ..Inspector::default()
            }),
            ..Workspace::default()
        };
        let history = [Entry {
            role: "assistant".into(),
            text: "CHAT_STAYS_VISIBLE 👩\u{200d}💻 e\u{301} 🇷🇺".into(),
            action: None,
            edit: None,
            status: None,
        }];
        let before = carbon::layout_workspace(
            &history,
            b"draft",
            3,
            "",
            0,
            "project",
            (120, 24),
            color,
            &ui,
        )
        .unwrap();
        // The divider is anchored even when a terminal composes emoji scalars.
        assert!(before.paint(0).contains("\r\x1b[69C"));
        ui.inspector.as_mut().unwrap().offset = 1;
        let after = carbon::layout_workspace(
            &history,
            b"draft",
            3,
            "",
            0,
            "project",
            (120, 24),
            color,
            &ui,
        )
        .unwrap();
        let delta = after.paint_changes(&before);
        assert!(!delta.contains("CHAT_STAYS_VISIBLE"));
        assert!(!delta.contains("\x1b[2K") && !delta.contains("\x1b[2J"));
        let changed = touched(&delta, &before);
        assert!(!changed.is_empty());
        // At 120 columns, the right pane starts at column 72 (zero based).
        assert!(
            changed
                .iter()
                .all(|&(row, column)| row >= 3 && column >= 72)
        );
        assert!(!delta.contains("👩\u{200d}💻"));

        let chat_row = before
            .plain_lines()
            .iter()
            .position(|line| line.contains("CHAT_STAYS_VISIBLE"))
            .unwrap();
        let mut selected_before = Selection::new(before, 0, chat_row);
        let mut selected_after = Selection::new(after, 0, chat_row);
        selected_before.drag(30, chat_row);
        selected_after.drag(30, chat_row);
        let copied = selected_before.text().unwrap();
        let before = selected_before.frame(color);
        let after = selected_after.frame(color);
        assert!(before.paint(0).contains("\r\x1b[69C"));
        assert!(
            touched(&after.paint_changes(&before), &before)
                .iter()
                .all(|&(_, column)| column >= 72)
        );
        for size in [(120, 24), (100, 24), (140, 30)] {
            let before = selected_before.frame_at(size, color);
            let after = selected_after.frame_at(size, color);
            assert!(before.paint(0).contains("\r\x1b[69C"));
            let delta = after.paint_changes(&before);
            assert!(!delta.contains("CHAT_STAYS_VISIBLE") && !delta.contains("👩\u{200d}💻"));
            let changed = touched(&delta, &before);
            assert!(!changed.is_empty());
            assert!(changed.iter().all(|&(_, column)| column >= 72));
        }
        let narrow = selected_before.frame_at((40, 12), color);
        assert!(!narrow.paint(0).contains("\x1b[69C"));
        assert_eq!(selected_before.text().unwrap(), copied);
    }
}

#[test]
fn style_changes_repaint_the_changed_glyph_and_restore_its_sgr() {
    let before = frame("stable \x1b[38;2;10;20;30mright\x1b[0m");
    let after = frame("stable \x1b[38;2;30;20;10mright\x1b[0m");
    let delta = after.paint_changes(&before);
    assert!(!delta.contains("stable"));
    assert!(delta.contains("\x1b[38;2;30;20;10mright"));
    assert!(
        touched(&delta, &before)
            .iter()
            .all(|&(row, column)| row == 4 && column >= 7)
    );
    // Style continuity must also hold when the differing text is inside a span.
    let before = frame("\x1b[1mstable old\x1b[0m");
    let after = frame("\x1b[1mstable new\x1b[0m");
    let delta = after.paint_changes(&before);
    assert!(delta.contains("\x1b[1mnew"));
    assert!(
        touched(&delta, &before)
            .iter()
            .all(|&(_, column)| column >= 7)
    );
    let before = frame("stable 👩\u{200d}💻 old");
    let after = frame("stable 👩\u{200d}💻 new");
    let delta = after.paint_changes(&before);
    assert!(!delta.contains("stable"));
    assert!(!delta.contains("👩\u{200d}💻"));
    assert!(delta.contains("\r\x1b[10C\x1b[Knew"));
    assert!(
        touched(&delta, &before)
            .iter()
            .all(|&(_, column)| column >= 10)
    );
    let before = frame("prefix \x1b[1m❤\x1b[0m\u{fe0f}old");
    let after = frame("prefix \x1b[1m❤\x1b[0m\u{fe0f}new");
    let delta = after.paint_changes(&before);
    assert!(!delta.contains('❤'));
    assert!(delta.contains("\r\x1b[9C"));
    assert!(
        touched(&delta, &before)
            .iter()
            .all(|&(_, column)| column >= 9)
    );
}

#[test]
fn repaint_boundaries_keep_combining_marks_and_wide_characters_whole() {
    let before = frame("Ae\u{300}界old");
    let after = frame("Ae\u{301}界new");
    let delta = after.paint_changes(&before);
    assert!(delta.contains("e\u{301}界new"));
    assert!(
        touched(&delta, &before)
            .iter()
            .all(|&(row, column)| row == 4 && column >= 1)
    );
    let before = frame("ab界old");
    let after = frame("abX");
    let delta = after.paint_changes(&before);
    assert!(delta.contains("\r\x1b[2C\x1b[KX"));
    assert!(
        touched(&delta, &before)
            .iter()
            .all(|&(_, column)| column >= 2)
    );
    // A changed character after ZWJ repaints the entire joined sequence.
    let before = frame("prefix 👩\u{200d}💻old");
    let after = frame("prefix 👩\u{200d}🔧new");
    let delta = after.paint_changes(&before);
    assert!(delta.contains("👩\u{200d}🔧new"));
    assert!(!delta.contains("prefix"));
    assert!(
        touched(&delta, &before)
            .iter()
            .all(|&(row, column)| row == 4 && column >= 7)
    );
    for cluster in ["❤️", "1️⃣", "👩\u{200d}💻", "🇷🇺"] {
        assert_eq!(cells::graphemes(cluster).count(), 1);
        assert_eq!(cells::width(cluster), 2);
        for text in [format!("{}{cluster}", "a".repeat(68)), cluster.repeat(69)] {
            for color in [false, true] {
                let ui = Workspace {
                    inspector: Some(Inspector {
                        title: "src/file.rs".into(),
                        lines: vec!["right pane".into()],
                        ..Inspector::default()
                    }),
                    ..Workspace::default()
                };
                let history = [Entry {
                    role: "assistant".into(),
                    text: text.clone(),
                    action: None,
                    edit: None,
                    status: None,
                }];
                let frame = carbon::layout_workspace(
                    &history,
                    b"",
                    0,
                    "",
                    0,
                    "project",
                    (120, 24),
                    color,
                    &ui,
                )
                .unwrap();
                let (first, last, _) = frame.chat_bounds.unwrap();
                let lines = frame.plain_lines();
                let left: Vec<_> = lines[first..last]
                    .iter()
                    .map(|line| line.split_once(" │ ").unwrap().0)
                    .collect();
                assert!(left.iter().all(|line| cells::width(line) == 69));
                assert_eq!(
                    left.iter().map(|line| line.trim_end()).collect::<String>(),
                    text
                );
                if text.starts_with('a') {
                    assert_eq!(left[0].trim_end(), "a".repeat(68));
                    assert_eq!(left[1].trim_end(), cluster);
                }
            }
        }
    }
    let selected = frame("prefix ❤️1️⃣ tail");
    assert_eq!(selected.selected_text((4, 8), (4, 8)), "❤️");
    assert_eq!(selected.selected_text((4, 10), (4, 10)), "1️⃣");
    let mut selection = Selection::new(selected, 8, 4);
    selection.drag(9, 4);
    assert_eq!(selection.text().unwrap(), "❤️1️⃣");
    let _clipped = selection.frame_at((10, 12), true);
    assert_eq!(selection.text().unwrap(), "❤️1️⃣");

    // Inline styling cannot split an emoji into separately measured scalars.
    let styled = carbon::layout_workspace(
        &[Entry {
            role: "assistant".into(),
            text: "**❤**\u{fe0f}1️⃣".into(),
            action: None,
            edit: None,
            status: None,
        }],
        b"",
        0,
        "",
        0,
        "project",
        (80, 24),
        true,
        &Workspace::default(),
    )
    .unwrap();
    assert!(styled.lines.iter().any(|line| line.contains("❤️")));
    assert!(
        styled
            .plain_lines()
            .iter()
            .any(|line| line.contains("❤️1️⃣"))
    );
    let mut input = vec![0x80, 0xff];
    input.extend_from_slice("❤️1️⃣\nlast".as_bytes());
    let editor = carbon::layout_workspace(
        &[],
        &input,
        2 + "❤️1️⃣".len(),
        "",
        0,
        "project",
        (80, 24),
        false,
        &Workspace::default(),
    )
    .unwrap();
    assert!(editor.lines.iter().any(|line| line == "› ??❤️1️⃣"));
    assert_eq!(editor.cursor, 8);
    let editor = carbon::layout_workspace(
        &[],
        "❤️".as_bytes(),
        "❤".len(),
        "",
        0,
        "project",
        (80, 24),
        false,
        &Workspace::default(),
    )
    .unwrap();
    assert_eq!(editor.cursor, 2);
}

#[test]
fn shrinking_a_line_clears_only_the_removed_suffix() {
    let mut before = frame("stable right tail");
    let mut after = frame("stable right");
    before.cursor = 0;
    after.cursor = 0;
    let delta = after.paint_changes(&before);
    assert!(!delta.contains("stable") && !delta.contains("right"));
    assert!(delta.contains("\r\x1b[12C\x1b[K"));
    assert!(!delta.contains("\x1b[0C"));
    assert!(
        touched(&delta, &before)
            .iter()
            .all(|&(row, column)| row == 4 && column >= 12)
    );
}

#[test]
fn resized_frames_still_invalidate_the_full_screen() {
    let before = frame("old");
    let after = carbon::layout_workspace(
        &[],
        b"draft",
        3,
        "",
        0,
        "project",
        (80, 24),
        false,
        &Workspace::default(),
    )
    .unwrap();
    assert!(after.paint_changes(&before).contains("\x1b[H\x1b[2J"));
}
