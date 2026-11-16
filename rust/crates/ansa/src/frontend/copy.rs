//! Explicit copy reads a bounded canonical prefix, without model/tool dispatch.
use super::protocol_v1::*;
pub(super) const LIMIT: usize = 64 * 1024;
#[derive(Default)]
pub(super) struct CopyHistory {
    total: usize,
    cursor: usize,
    active: bool,
    pending: Option<CommandId>,
    data: String,
    result: Option<Result<String, &'static str>>,
}
impl CopyHistory {
    pub fn start(&mut self, total: usize) -> Result<(), &'static str> {
        if self.active {
            return Err("Копирование уже выполняется");
        }
        if total == 0 {
            return Err("История пока пуста");
        }
        *self = Self {
            total,
            active: true,
            ..Self::default()
        };
        Ok(())
    }
    pub fn query(&self) -> Option<GetTranscript> {
        (self.active && self.pending.is_none()).then_some(GetTranscript {
            before: self.cursor,
            limit: 63,
            forward: true,
        })
    }
    pub fn sent(&mut self, command: CommandId) {
        self.pending = Some(command);
    }
    pub fn retry(&mut self, command: Option<&CommandId>) {
        if command.is_none_or(|id| self.pending.as_ref() == Some(id)) {
            self.pending = None;
        }
    }
    pub fn rejected(&mut self, command: &CommandId) {
        if self.active && self.pending.as_ref() == Some(command) {
            self.active = false;
            self.pending = None;
            self.data.clear();
            self.result = Some(Err("Историю не удалось прочитать"));
        }
    }
    pub fn apply(&mut self, page: &TranscriptPage) {
        if !self.active
            || self.pending.is_none()
            || page.query
                != (GetTranscript {
                    before: self.cursor,
                    limit: 63,
                    forward: true,
                })
        {
            return;
        }
        self.pending = None;
        if page.begin != self.cursor
            || page.end <= self.cursor
            || page.end - page.begin != page.entries.len()
            || page.total < self.total
        {
            self.active = false;
            self.result = Some(Err("Историю не удалось прочитать"));
            return;
        }
        for entry in page.entries.iter().take(self.total - self.cursor) {
            let role = if entry.role == "user" {
                "Вы:\n"
            } else {
                "ansa:\n"
            };
            if self.data.len() + role.len() + entry.text.len() + 2 > LIMIT {
                self.active = false;
                self.data.clear();
                self.result = Some(Err(
                    "Диалог больше 64 KiB: выделите нужный фрагмент терминалом",
                ));
                return;
            }
            self.data.push_str(role);
            self.data.push_str(&entry.text);
            self.data.push_str("\n\n");
        }
        self.cursor = page.end.min(self.total);
        if self.cursor == self.total {
            self.active = false;
            self.result = Some(Ok(std::mem::take(&mut self.data)));
        }
    }
    pub fn take(&mut self) -> Option<Result<String, &'static str>> {
        self.result.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page(text: String) -> TranscriptPage {
        TranscriptPage {
            begin: 0,
            end: 1,
            total: 2,
            query: GetTranscript {
                before: 0,
                limit: 63,
                forward: true,
            },
            entries: vec![crate::session::transcript::Entry {
                tool: None,
                turn: "t".into(),
                request: "r".into(),
                role: "user".into(),
                text,
                state: "completed".into(),
                mode: AgentMode::Basic,
            }],
        }
    }
    #[test]
    fn copy_is_exactly_once_and_excludes_newer_rows() {
        let mut copy = CopyHistory::default();
        copy.start(1).unwrap();
        copy.sent("read".into());
        let p = page("Ж😀".into());
        copy.apply(&p);
        copy.apply(&p);
        assert_eq!(copy.take().unwrap().unwrap(), "Вы:\nЖ😀\n\n");
        assert!(copy.take().is_none());
        assert!(copy.query().is_none());
    }
    #[test]
    fn oversized_copy_fails_without_truncation_or_clipboard_result() {
        let mut copy = CopyHistory::default();
        copy.start(1).unwrap();
        copy.sent("read".into());
        copy.apply(&page("x".repeat(LIMIT)));
        assert!(copy.take().unwrap().is_err());
        assert!(copy.data.is_empty());
    }
}
