//! Input recall reads bounded canonical pages; it never writes a second history.
use super::{input::Editor, protocol_v1::*};

pub(super) struct Draft {
    bytes: Vec<u8>,
    cursor: usize,
    pasted_ranges: Vec<std::ops::Range<usize>>,
}
impl Draft {
    pub(super) fn capture(editor: &Editor) -> Self {
        Self {
            bytes: editor.bytes.clone(),
            cursor: editor.cursor,
            pasted_ranges: editor.pasted_ranges.clone(),
        }
    }

    pub(super) fn restore(self, editor: &mut Editor) {
        editor.clear();
        editor.bytes = self.bytes;
        editor.cursor = self.cursor;
        editor.pasted_ranges = self.pasted_ranges;
    }
}
struct Pending {
    query: GetTranscript,
    older: bool,
    command: Option<CommandId>,
}
#[derive(Default)]
pub(super) struct History {
    draft: Option<Draft>,
    anchor: usize,
    selected: usize,
    target: usize,
    index: Option<usize>,
    oldest: bool,
    pending: Option<Pending>,
}
impl History {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn navigate(&mut self, older: bool, editor: &mut Editor, total: usize) {
        if editor.overflow || total == 0 {
            return;
        }
        if self.draft.is_none() {
            if !older {
                return;
            }
            self.draft = Some(Draft::capture(editor));
            self.anchor = total;
        }
        if older {
            if self.oldest && self.target == self.selected {
                return;
            }
            self.target = self.target.saturating_add(1).min(self.anchor);
        } else {
            self.target = self.target.saturating_sub(1);
        }
        if self.target == 0 {
            let draft = self.draft.take().expect("active recall");
            draft.restore(editor);
            self.reset();
        }
    }
    pub fn edited(&mut self, editor: &Editor) {
        // An in-flight page must not overwrite typing or a moved cursor.
        self.pending = None;
        self.target = self.selected;
        if self.selected == 0 && self.draft.is_some() {
            self.draft = Some(Draft::capture(editor));
        }
    }
    pub fn query(&mut self) -> Option<GetTranscript> {
        if self.target == self.selected {
            return None;
        }
        let older = self.target > self.selected;
        if self.pending.as_ref().is_some_and(|p| p.older != older) {
            self.pending = None;
        }
        if self.pending.is_none() {
            let before = if older {
                self.index.unwrap_or(self.anchor)
            } else {
                self.index.expect("selected history entry") + 1
            };
            if older && before == 0 {
                self.oldest = true;
                self.target = self.selected;
                return None;
            }
            self.pending = Some(Pending {
                query: GetTranscript {
                    before,
                    limit: 64,
                    forward: !older,
                },
                older,
                command: None,
            });
        }
        self.pending
            .as_ref()
            .filter(|p| p.command.is_none())
            .map(|p| p.query.clone())
    }
    pub fn sent(&mut self, command: CommandId) {
        self.pending.as_mut().expect("pending recall page").command = Some(command);
    }
    pub fn retry(&mut self, command: Option<&CommandId>) {
        if let Some(pending) = &mut self.pending
            && command.is_none_or(|id| pending.command.as_ref() == Some(id))
        {
            pending.command = None;
        }
    }
    pub fn apply(&mut self, page: &TranscriptPage, editor: &mut Editor) {
        let Some(pending) = &self.pending else {
            return;
        };
        if pending.command.is_none() || pending.query != page.query {
            return;
        }
        let older = self.target > self.selected;
        if self.target == self.selected || pending.older != older {
            self.pending = None;
            return;
        }
        let found = |(i, entry): &(usize, &crate::session::transcript::Entry)| {
            entry.role == "user" && page.begin + i < self.anchor
        };
        let mut entries = page.entries.iter().enumerate();
        let selected = if older {
            entries.rfind(found)
        } else {
            entries.find(found)
        };
        if let Some((offset, entry)) = selected {
            if entry.text.len() > super::input::MAX_COMPOSER_BYTES {
                self.pending = None;
                self.target = self.selected;
                return;
            }
            editor.clear();
            editor.bytes = entry.text.as_bytes().to_vec();
            editor.cursor = editor.bytes.len();
            self.index = Some(page.begin + offset);
            self.selected = if older {
                self.selected + 1
            } else {
                self.selected - 1
            };
            self.oldest = self.index == Some(0);
            self.pending = None;
        } else if older && page.begin > 0 || !older && page.end < self.anchor {
            let pending = self.pending.as_mut().expect("matched page");
            pending.query.before = if older { page.begin } else { page.end };
            pending.command = None;
        } else {
            self.target = self.selected;
            self.oldest = older;
            self.pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::transcript::{Entry, Transcript};
    fn transcript() -> Transcript {
        let mut transcript = Transcript::default();
        for i in 0..80 {
            for role in ["user", "assistant"] {
                transcript.append(Entry {
                    tool: None,
                    turn: TurnId(format!("t{i}")),
                    request: RequestId(format!("r{i}")),
                    role: role.into(),
                    text: format!("{role} {i} Ж😀"),
                    state: "completed".into(),
                    mode: AgentMode::Basic,
                });
            }
        }
        transcript
    }
    fn response(history: &mut History, transcript: &Transcript, editor: &mut Editor) {
        let query = history.query().unwrap();
        history.sent("read".into());
        let page = transcript.page(query);
        history.apply(
            &TranscriptPage {
                begin: page.begin,
                end: page.end,
                total: page.total,
                entries: page.entries,
                query: page.query,
            },
            editor,
        );
    }
    #[test]
    fn recall_crosses_pages_and_returns_the_exact_draft_cursor() {
        let transcript = transcript();
        let mut editor = Editor::default();
        editor.bytes = "черновик😀".as_bytes().to_vec();
        editor.cursor = 4;
        let mut history = History::default();
        for i in (0..80).rev() {
            history.navigate(true, &mut editor, transcript.len());
            response(&mut history, &transcript, &mut editor);
            assert_eq!(editor.text().unwrap(), format!("user {i} Ж😀"));
        }
        history.navigate(true, &mut editor, transcript.len());
        assert!(history.query().is_none());
        for i in 1..80 {
            history.navigate(false, &mut editor, transcript.len());
            response(&mut history, &transcript, &mut editor);
            assert_eq!(editor.text().unwrap(), format!("user {i} Ж😀"));
        }
        history.navigate(false, &mut editor, transcript.len());
        assert_eq!(editor.text().unwrap(), "черновик😀");
        assert_eq!(editor.cursor, 4);
    }
    #[test]
    fn late_pages_cannot_overwrite_typing_or_a_reversed_arrow() {
        let transcript = transcript();
        let mut editor = Editor::default();
        let mut history = History::default();
        history.navigate(true, &mut editor, transcript.len());
        let query = history.query().unwrap();
        history.sent("old".into());
        let p = transcript.page(query);
        let page = TranscriptPage {
            begin: p.begin,
            end: p.end,
            total: p.total,
            entries: p.entries,
            query: p.query,
        };
        editor.feed(b'x', true);
        history.edited(&editor);
        history.apply(&page, &mut editor);
        assert_eq!(editor.text().unwrap(), "x");
        // Explicit Enter can be rejected as busy without modifying the editor.
        history.navigate(true, &mut editor, transcript.len());
        history.query();
        history.sent("busy".into());
        history.edited(&editor);
        history.apply(&page, &mut editor);
        assert_eq!(editor.text().unwrap(), "x");
        history.navigate(true, &mut editor, transcript.len());
        history.query();
        history.sent("next".into());
        history.navigate(false, &mut editor, transcript.len());
        history.apply(&page, &mut editor);
        assert_eq!(editor.text().unwrap(), "x");
        assert!(history.query().is_none());
    }
    #[test]
    fn repeated_arrows_resync_and_edits_preserve_the_draft() {
        let transcript = transcript();
        let mut editor = Editor::default();
        editor.feed(b'd', true);
        let mut history = History::default();
        history.navigate(true, &mut editor, transcript.len());
        history.navigate(true, &mut editor, transcript.len());
        let first = history.query().unwrap();
        history.sent("lost".into());
        assert!(history.query().is_none());
        history.retry(None);
        assert_eq!(history.query().unwrap(), first);
        response(&mut history, &transcript, &mut editor);
        response(&mut history, &transcript, &mut editor);
        assert_eq!(editor.text().unwrap(), "user 78 Ж😀");
        editor.feed(b'!', true);
        history.edited(&editor);
        history.navigate(false, &mut editor, transcript.len());
        response(&mut history, &transcript, &mut editor);
        history.navigate(false, &mut editor, transcript.len());
        assert_eq!(editor.text().unwrap(), "d");
    }
    #[test]
    fn overflow_cannot_be_healed_by_history_recall() {
        let mut editor = Editor::default();
        editor
            .bytes
            .resize(crate::frontend::input::MAX_COMPOSER_BYTES, b'x');
        editor.cursor = editor.bytes.len();
        editor.feed(b'x', true);
        let mut history = History::default();
        history.navigate(true, &mut editor, 5);
        assert!(history.query().is_none());
        assert!(editor.overflow);
        assert_eq!(
            editor.bytes.len(),
            crate::frontend::input::MAX_COMPOSER_BYTES
        );
    }
}
