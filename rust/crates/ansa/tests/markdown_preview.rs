use ansa::frontend::{cells, markdown::Document};

fn texts(document: &Document, columns: usize) -> Vec<String> {
    document
        .rows(columns)
        .iter()
        .map(|row| row.plain())
        .collect()
}

#[test]
fn markdown_blocks_have_semantic_styles_and_readable_plain_text() {
    let document = Document::parse(
        "# Heading\n\nA **strong** and *soft* [label](https://example.invalid/long/path).\n\n\
         > Quoted `a_b`\n\n- [x] ready\n- [ ] later\n\n\
         | Name | State |\n| --- | --- |\n| item | ~~old~~ |\n",
    );
    let plain = document.plain();
    assert!(plain.starts_with("Heading\n\nA strong and soft label."));
    assert!(!plain.contains("https://"));
    assert!(!plain.contains("**"));
    assert!(plain.contains("> Quoted a_b"));
    assert!(plain.contains("- [x] ready\n- [ ] later"));
    assert!(plain.contains("Name | State\nitem | old"));
    let rows = document.rows(100);
    assert!(rows[0].spans.iter().all(|span| span.style.heading == 1));
    let spans = rows.iter().flat_map(|row| &row.spans).collect::<Vec<_>>();
    assert!(
        spans
            .iter()
            .any(|span| span.text == "strong" && span.style.bold)
    );
    assert!(
        spans
            .iter()
            .any(|span| span.text == "soft" && span.style.italic)
    );
    assert!(
        spans
            .iter()
            .any(|span| span.text == "label" && span.style.link)
    );
    assert!(
        spans
            .iter()
            .any(|span| span.text == "a_b" && span.style.code)
    );
    assert!(
        spans
            .iter()
            .any(|span| span.text == "old" && span.style.strike)
    );
}

#[test]
fn fences_and_code_spans_preserve_markdown_literals_and_whitespace() {
    let inline = Document::parse("`  hello`\n");
    assert!(inline.rows(20)[0].plain().starts_with("  hello"));
    assert_eq!(
        inline
            .rows(4)
            .iter()
            .map(|row| row.plain())
            .collect::<String>(),
        "  hello"
    );
    let document = Document::parse(
        "## Examples\n\n````rust\n\tlet x = \"**raw** `x`\";\n```\n\nend();\n````\n\n\
         ``a `b` **c**`` and \\*literal\\* and path_name\n",
    );
    let plain = document.plain();
    assert!(
        plain.contains("rust\n  \tlet x = \"**raw** `x`\";\n  ```\n  \n  end();"),
        "{plain:?}"
    );
    assert!(plain.contains("a `b` **c** and *literal* and path_name"));
    let rows = document.rows(200);
    let code = rows
        .iter()
        .find(|row| row.plain().contains("let x"))
        .unwrap();
    assert!(code.plain().starts_with("      let x"));
    assert_eq!(code.source_line, 3);
    assert!(
        code.spans
            .iter()
            .filter(|span| !span.text.trim().is_empty())
            .all(|span| span.style.code)
    );
    assert!(rows.iter().any(|row| row.plain() == "  ```"));
    assert!(
        Document::parse("```\nx\n\n```\n")
            .plain()
            .ends_with("  x\n  \n")
    );
}

#[test]
fn nested_lists_wrap_with_hanging_indent_and_source_mapping() {
    let document = Document::parse(
        "1. alpha beta gamma delta epsilon\n   continuation source words\n   - nested words keep their indent\n2. final\n",
    );
    let rows = document.rows(20);
    let plain: Vec<String> = rows.iter().map(|row| row.plain()).collect();
    assert!(plain[0].starts_with("1. alpha beta"), "{plain:?}");
    assert!(plain[1].starts_with("   delta"), "{plain:?}");
    let nested = plain
        .iter()
        .position(|line| line.starts_with("   - nested"))
        .unwrap();
    assert!(plain[nested + 1].starts_with("     "));
    assert!(plain.last().unwrap().starts_with("2. final"));
    assert!(rows.iter().any(|row| row.source_line == 1));
    assert!(rows.iter().any(|row| row.source_line == 2));
    assert_eq!(rows.last().unwrap().source_line, 3);
    assert!(plain.iter().all(|line| line.chars().count() <= 20));
}

#[test]
fn cell_wrapping_preserves_wide_combining_and_long_code_contents() {
    let document = Document::parse("界界 e\u{301} abcdefghijk\n\n```\n0123456789  XYZ\n```\n");
    let rows = document.rows(8);
    for row in &rows {
        assert!(cells::width(&row.plain()) <= 8, "{:?}", row.plain());
        assert!(!row.plain().starts_with('\u{301}'));
    }
    let text = rows
        .iter()
        .map(|row| row.plain())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("界界 e\u{301}"), "{text:?}");
    assert!(text.contains("abcdef"));
    let code: String = rows
        .iter()
        .flat_map(|row| row.spans.iter())
        .filter(|span| span.style.code)
        .map(|span| span.text.as_str())
        .collect();
    assert_eq!(code, "0123456789  XYZ");
    assert_eq!(texts(&Document::parse("a bcd    \n"), 5), ["a bcd"]);
    assert!(
        texts(&document, 1)
            .iter()
            .all(|line| cells::width(line) <= 1)
    );

    for cluster in ["❤️", "1️⃣", "👩‍💻", "🇷🇺", "👨‍👩‍👧‍👦"] {
        assert_eq!(cells::width(cluster), 2, "{cluster:?}");
        let content = format!("AAA{cluster}ZZZ{cluster}end");
        let document = Document::parse(&format!("```\n{content}\n```\n"));
        for columns in [4, 5, 6, 8, 12] {
            let rows = document.rows(columns);
            assert!(rows.iter().all(|row| cells::width(&row.plain()) <= columns));
            let code: String = rows
                .iter()
                .flat_map(|row| &row.spans)
                .filter(|span| span.style.code)
                .map(|span| span.text.as_str())
                .collect();
            assert_eq!(code, content, "{cluster:?} at {columns}");
            assert_eq!(
                rows.iter()
                    .map(|row| cells::graphemes(&row.plain())
                        .filter(|grapheme| *grapheme == cluster)
                        .count())
                    .sum::<usize>(),
                2
            );
        }
        let narrow = document.rows(5);
        let wide = document.rows(8);
        for row in &narrow {
            let index = ansa::frontend::markdown::Row::locate(&wide, row.anchor);
            assert!(wide[index].anchor <= row.anchor);
            assert!(
                wide.get(index + 1)
                    .is_none_or(|next| next.anchor > row.anchor)
            );
        }
    }
    let crossing = Document::parse("`❤`\u{fe0f} next").rows(2);
    assert_eq!(crossing[0].plain(), "❤️");
    assert!(crossing[0].spans.iter().all(|span| span.style.code));
}

#[test]
fn decoded_entities_html_and_link_labels_cannot_emit_terminal_controls() {
    let document = Document::parse(
        "# &#x1b;[31m\n\n[label&#x202e;](javascript:ignored) ![alt](remote.png)\n\n\
         <script>\n\x1b]52;c;evil\x07\n</script>\n\n`\u{2066}literal`\n",
    );
    let plain = document.plain();
    assert!(!plain.contains('\x1b'));
    assert!(!plain.contains('\x07'));
    assert!(!plain.contains('\u{202e}'));
    assert!(!plain.contains('\u{2066}'));
    assert!(plain.contains("\\u{1b}[31m"));
    assert!(plain.contains("label\\u{202e}"));
    assert!(plain.contains("[alt]"));
    assert!(plain.contains("<script>"));
    assert!(!plain.contains("javascript:"));
    assert!(!plain.contains("remote.png"));
    for row in document.rows(18) {
        assert!(!row.plain().chars().any(char::is_control));
    }
}

#[test]
fn deep_blocks_and_large_paragraphs_keep_tail_and_bounded_cell_width() {
    let mut source = format!("{}nested\n\n", "> ".repeat(256));
    source.push_str(&"word ".repeat(12_000));
    source.push_str("\n\n## Tail marker\n");
    let document = Document::parse(&source);
    for columns in [1, 12, 41, 100] {
        let rows = document.rows(columns);
        assert!(
            rows.iter()
                .all(|row| { cells::width(&row.plain()) <= columns })
        );
        let tail: String = rows
            .iter()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|row| row.plain())
            .collect();
        assert!(
            tail.replace(' ', "").ends_with("Tailmarker"),
            "{columns}: {tail:?}"
        );
    }
}

#[test]
fn navigation_keeps_a_wrapped_paragraph_anchor_and_finds_joined_source_lines() {
    use ansa::frontend::markdown::Row;
    let document = Document::parse(&format!(
        "{}\n\n# Tail\n",
        (0..300).map(|i| format!("word{i} ")).collect::<String>()
    ));
    let narrow = document.rows(35);
    let anchor = narrow[40].anchor;
    let wider = document.rows(52);
    let index = Row::locate(&wider, anchor);
    assert!(index > 15);
    assert!(wider[index].anchor <= anchor);
    assert!(wider[index + 1].anchor > anchor);
    let joined = Document::parse("alpha\nbeta\n\n# Tail\n").rows(80);
    assert_eq!(Row::source_row(&joined, 1), 0);
    assert_eq!(joined[0].plain(), "alpha beta");
    assert_eq!(joined[Row::source_row(&joined, 3)].plain(), "Tail");
}
