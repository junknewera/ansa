//! Workspace interactions. Typed runtime state remains the source of truth.
use super::*;
use crate::frontend::terminal_input::TerminalEvent;

impl Workbench {
    pub(super) fn navigating(&self) -> bool {
        self.menu.is_some()
            || (self.focus == Focus::Inspector && !matches!(self.panel, Some(Panel::Terminal)))
    }
    pub(super) fn input_editor(&self) -> &crate::frontend::input::Editor {
        if self.navigating() {
            &self.navigation_editor
        } else {
            &self.slots[self.current].state.editor
        }
    }
    pub(super) fn input_editor_mut(&mut self) -> &mut crate::frontend::input::Editor {
        if self.navigating() {
            &mut self.navigation_editor
        } else {
            &mut self.slots[self.current].state.editor
        }
    }
    pub(super) fn available_commands(&self) -> &'static [commands::Command] {
        commands::available(
            self.factory
                .terminal_owner(&self.slots[self.current].id)
                .is_some(),
        )
    }
    pub(super) fn refresh_completion(&mut self) {
        if self.menu.is_some()
            || self.focus != Focus::Chat
            || self.selection.as_ref().is_some_and(Selection::is_chat)
        {
            self.completion = None;
            return;
        }
        let editor = &self.slots[self.current].state.editor;
        if self
            .completion_closed
            .as_ref()
            .is_some_and(|(bytes, cursor)| *bytes == editor.bytes && *cursor == editor.cursor)
        {
            self.completion = None;
            return;
        }
        self.completion_closed = None;
        let mut next = Completion::at_with_commands(editor, self.available_commands());
        if let (Some(old), Some(next)) = (&self.completion, &mut next)
            && old.begin == next.begin
            && old.query == next.query
        {
            next.selected = old.selected;
        }
        self.completion = next;
    }
    pub(super) fn complete_command(&mut self) {
        if let Some(completion) = self.completion.take() {
            let slot = &mut self.slots[self.current];
            if completion.apply(&mut slot.state.editor) {
                slot.state.input_history.edited(&slot.state.editor);
            }
        }
        let editor = &self.slots[self.current].state.editor;
        self.completion_closed = Some((editor.bytes.clone(), editor.cursor));
    }
    pub(super) fn title(&self, id: &str) -> String {
        self.known_title(id)
            .unwrap_or_else(|| "Новый разговор".into())
    }
    pub(super) fn known_title(&self, id: &str) -> Option<String> {
        self.slots
            .iter()
            .find(|slot| slot.id == id)
            .and_then(|slot| slot.title.clone())
            .or_else(|| {
                self.summaries
                    .iter()
                    .find(|summary| summary.entry.id == id)
                    .and_then(|summary| summary.title.clone())
            })
    }
    pub(super) fn conversation_root(&self) -> String {
        let mut id = self.slots[self.current].id.clone();
        for _ in 0..128 {
            if let Some(slot) = self.slots.iter().find(|slot| slot.id == id) {
                if matches!(slot.kind, Kind::Main | Kind::Fork) {
                    break;
                }
                if let Some(parent) = &slot.parent {
                    id = parent.clone();
                    continue;
                }
            } else if let Some(summary) =
                self.summaries.iter().find(|summary| summary.entry.id == id)
            {
                if matches!(summary.entry.kind.as_str(), "main" | "fork") {
                    break;
                }
                if let Some(parent) = &summary.entry.parent {
                    id = parent.clone();
                    continue;
                }
            }
            break;
        }
        id
    }
    pub(super) fn agent_rows(&self) -> Vec<(String, carbon::AgentLine)> {
        let root = self.conversation_root();
        let mut candidates: Vec<(String, Option<String>, String)> = self
            .summaries
            .iter()
            .map(|s| {
                (
                    s.entry.id.clone(),
                    s.entry.parent.clone(),
                    s.entry.kind.clone(),
                )
            })
            .collect();
        for slot in &self.slots {
            if !candidates.iter().any(|(id, _, _)| *id == slot.id) {
                candidates.push((
                    slot.id.clone(),
                    slot.parent.clone(),
                    slot.kind.name().into(),
                ));
            }
        }
        let mut result = Vec::new();
        let mut pending = vec![(root.clone(), String::new(), String::new())];
        let mut seen = std::collections::BTreeSet::new();
        while let Some((parent, tree, branch)) = pending.pop() {
            if !seen.insert(parent.clone()) || seen.len() > 128 {
                continue;
            }
            let children: Vec<_> = candidates
                .iter()
                .filter(|(_, candidate_parent, kind)| {
                    candidate_parent.as_ref() == Some(&parent) && kind != "fork"
                })
                .collect();
            // Depth-first order keeps a child's rows adjacent to its parent.
            let indent = format!(
                "{tree}{}",
                match branch.as_str() {
                    "├─ " => "│  ",
                    "└─ " => "   ",
                    _ => "",
                }
            );
            let mut visible_after = false;
            for (id, _, kind) in children.into_iter().rev() {
                let connector = if kind != "agent" {
                    ""
                } else if visible_after {
                    "├─ "
                } else {
                    "└─ "
                };
                visible_after |= kind == "agent";
                pending.push((id.clone(), indent.clone(), connector.into()));
            }
            if parent == root {
                continue;
            }
            let Some((id, _, kind)) = candidates.iter().find(|(id, _, _)| *id == parent) else {
                continue;
            };
            if kind != "agent" {
                continue;
            }
            let live = self.slots.iter().find(|slot| slot.id == *id);
            let state = live
                .and_then(|slot| slot.state.snapshot.as_ref())
                .map(|view| {
                    if view.worker_busy {
                        "Работает"
                    } else {
                        match view.state.as_str() {
                            "completed" => "Завершён",
                            "cancelled" => "Остановлен",
                            "paused" => "Пауза",
                            _ => "Готов",
                        }
                    }
                })
                .unwrap_or_else(|| {
                    self.summaries
                        .iter()
                        .find(|s| s.entry.id == *id)
                        .map_or("Недоступен", |s| s.status.label())
                });
            let generation = live
                .and_then(|slot| slot.state.snapshot.as_ref())
                .map(|view| {
                    format!(
                        "{}{}",
                        view.generation.model,
                        view.generation
                            .effort
                            .as_ref()
                            .map_or(String::new(), |effort| format!(" · {effort}"))
                    )
                })
                .unwrap_or_default();
            let elapsed = live.and_then(|slot| slot.state.progress.elapsed());
            let detail = match (generation.is_empty(), elapsed) {
                (false, Some(elapsed)) => format!("{indent}{generation} · {elapsed}"),
                (false, None) => format!("{indent}{generation}"),
                (true, Some(elapsed)) => format!("{indent}{elapsed}"),
                (true, None) => String::new(),
            };
            result.push((
                id.clone(),
                carbon::AgentLine {
                    title: format!("{tree}{branch}{}", self.title(id)),
                    state: state.into(),
                    detail,
                },
            ));
        }
        result
    }
    pub(super) fn show_models(&mut self, models: Vec<(String, String, Vec<String>)>, effort: bool) {
        if !effort
            && let Some(requested) = self.requested_model.take()
            && let Some(selected) = models.iter().position(|model| model.0 == requested)
        {
            self.choose_model_effort(models, selected);
            return;
        }
        let current = self.slots[self.current]
            .state
            .snapshot
            .as_ref()
            .map(|view| view.generation.clone());
        let selected = current
            .as_ref()
            .and_then(|current| models.iter().position(|m| m.0 == current.model))
            .unwrap_or(0);
        if effort {
            let model = current
                .as_ref()
                .map(|g| g.model.clone())
                .unwrap_or_else(|| self.factory.settings.model.clone());
            let supported = models
                .iter()
                .find(|m| m.0 == model)
                .map(|m| m.2.clone())
                .unwrap_or_default();
            let mut values = vec![None];
            values.extend(supported.into_iter().map(Some));
            let selected = current
                .as_ref()
                .and_then(|g| values.iter().position(|effort| *effort == g.effort))
                .unwrap_or(0);
            self.menu = Some(Menu::Efforts {
                model,
                values,
                selected,
                previous: None,
            });
        } else {
            self.menu = Some(Menu::Models { models, selected });
        }
    }
    pub(super) fn choose_model_effort(
        &mut self,
        models: Vec<(String, String, Vec<String>)>,
        selected: usize,
    ) {
        let Some(model) = models.get(selected) else {
            return;
        };
        let name = model.0.clone();
        let mut values = vec![None];
        values.extend(model.2.iter().cloned().map(Some));
        let current = self.slots[self.current]
            .state
            .snapshot
            .as_ref()
            .and_then(|view| view.generation.effort.as_ref());
        let effort = current
            .and_then(|value| values.iter().position(|e| e.as_ref() == Some(value)))
            .unwrap_or(0);
        self.menu = Some(Menu::Efforts {
            model: name,
            values,
            selected: effort,
            previous: Some((models, selected)),
        });
    }
    pub(super) fn apply_generation(&mut self, model: String, effort: Option<String>, io: &Io) {
        let slot = &mut self.slots[self.current];
        if slot.state.post(
            &slot.handle,
            CommandPayload::SetGeneration(GenerationSelection { model, effort }),
            &mut slot.replies,
        ) {
            slot.state
                .report(io, "Применяю модель и уровень рассуждения…");
        }
    }
    fn terminal_dimensions(&self) -> Option<(u16, u16)> {
        // A key can arrive after the Files footer has been written but before
        // the writer acknowledges the whole frame. Use the newest geometry.
        self.last_frame
            .as_ref()
            .or(self.displayed.as_ref())
            .and_then(|frame| frame.inspector_bounds())
            .filter(|(_, _, width, height)| *width > 0 && *height > 0)
            .map(|(_, _, width, height)| (height.clamp(1, 128) as u16, width.clamp(1, 256) as u16))
    }
    pub(super) fn terminal_focused(&self) -> bool {
        self.menu.is_none()
            && self.focus == Focus::Inspector
            && matches!(self.panel, Some(Panel::Terminal))
            && self.terminal_dimensions().is_some()
    }
    pub(super) fn open_terminal(&mut self, io: &Io) {
        let Some((rows, columns)) = self.terminal_dimensions() else {
            self.focus = Focus::Chat;
            self.report(io, "Увеличьте окно для панели терминала");
            return;
        };
        let directory = match &self.panel {
            Some(Panel::Files(files)) => files
                .document
                .as_ref()
                .and_then(|d| d.path.parent().map(Path::to_path_buf))
                .or_else(|| files.listing.as_ref().map(|listing| listing.path.clone())),
            _ => None,
        }
        .unwrap_or_else(|| self.factory.settings.cwd.clone());
        if self
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.snapshot().exited)
        {
            if let Some(mut terminal) = self.terminal.take() {
                terminal.shutdown();
            }
            self.terminal_grid = None;
            self.terminal_size = None;
        }
        if self.terminal.is_none() {
            match crate::frontend::terminal_pane::TerminalPane::spawn(&directory, rows, columns) {
                Ok(terminal) => self.terminal = Some(terminal),
                Err(error) => {
                    self.report(io, &format!("Не удалось открыть терминал: {error}"));
                    return;
                }
            }
        } else if let Some(terminal) = &self.terminal {
            let snapshot = terminal.snapshot();
            if snapshot.shell_ready
                && snapshot.cwd != directory
                && let Err(error) = terminal.change_directory(&directory)
            {
                self.report(
                    io,
                    &format!("Терминал остаётся в {}: {error}", snapshot.cwd.display()),
                );
            }
        }
        if let Some(Panel::Files(files)) = self.panel.take() {
            self.terminal_back = Some(files);
        }
        self.panel = Some(Panel::Terminal);
        self.focus = Focus::Inspector;
        self.selection = None;
        self.tool_press = None;
        self.completion = None;
    }
    pub(super) fn hide_terminal(&mut self, io: &Io) {
        let directory = self.terminal.as_ref().map(|terminal| {
            terminal.cancel_directory_change();
            terminal.snapshot().cwd.clone()
        });
        self.panel = self.terminal_back.take().map(Panel::Files);
        self.focus = Focus::Inspector;
        self.selection = None;
        self.tool_press = None;
        if let Some(directory) = directory {
            self.open_file(directory, io);
        } else {
            self.focus = Focus::Chat;
        }
    }
    pub(super) fn terminal_bytes(&mut self, bytes: &[u8], io: &Io) {
        if !bytes.is_empty() {
            self.selection = None;
            self.tool_press = None;
        }
        if bytes == [3] {
            self.terminal_pending_input.clear();
        }
        if !self.terminal_pending_input.is_empty() {
            if self.terminal_pending_input.len() + bytes.len() <= 64 * 1024 {
                self.terminal_pending_input.extend_from_slice(bytes);
            } else {
                self.report(io, "Терминал занят: дождитесь приёма ввода");
            }
            return;
        }
        let Some(terminal) = &self.terminal else {
            return;
        };
        for (index, part) in bytes.chunks(4096).enumerate() {
            match terminal.input(part) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    self.terminal_pending_input
                        .extend_from_slice(&bytes[index * 4096..]);
                    break;
                }
                Err(error) => {
                    self.report(io, &format!("Ввод терминала недоступен: {error}"));
                    break;
                }
            }
        }
    }
    pub(super) fn flush_terminal_input(&mut self, io: &Io) {
        if !self.terminal_pending_input.is_empty() {
            let pending = std::mem::take(&mut self.terminal_pending_input);
            self.terminal_bytes(&pending, io);
        }
    }
    pub(super) fn terminal_event(
        &mut self,
        event: TerminalEvent,
        io: &Io,
        deferred: &mut std::collections::VecDeque<u8>,
    ) {
        match event {
            TerminalEvent::Bytes(bytes) => {
                if self.terminal_focused() {
                    self.terminal_bytes(&bytes, io);
                } else {
                    deferred.extend(bytes);
                }
            }
            TerminalEvent::Escape => {
                if self.terminal_focused()
                    && self
                        .terminal
                        .as_ref()
                        .is_some_and(|terminal| terminal.snapshot().shell_ready)
                {
                    self.hide_terminal(io);
                } else if self.terminal_focused() {
                    self.terminal_bytes(b"\x1b", io);
                } else {
                    deferred.push_back(27);
                }
            }
            TerminalEvent::Mouse {
                event,
                button,
                release,
            } => {
                let bounds = self
                    .displayed
                    .as_ref()
                    .and_then(|frame| frame.inspector_bounds());
                let in_pane = bounds.is_some_and(|(x, y, width, height)| {
                    event.column >= x
                        && event.column < x + width
                        && event.row >= y
                        && event.row < y + height
                });
                let screen = self.terminal.as_ref().map(|terminal| terminal.snapshot());
                if in_pane
                    && button & 4 == 0
                    && screen.as_ref().is_some_and(|s| {
                        s.mouse_protocol != crate::frontend::terminal_pane::MouseProtocolMode::None
                    })
                {
                    let (x, y, _, _) = bounds.expect("pane hit");
                    let screen = screen.expect("terminal exists");
                    let column = event.column - x + 1;
                    let row = event.row - y + 1;
                    let bytes = terminal_mouse(
                        button,
                        release,
                        column,
                        row,
                        screen.mouse_protocol,
                        screen.mouse_encoding,
                    );
                    if let Some(bytes) = bytes {
                        self.terminal_bytes(&bytes, io);
                    }
                } else {
                    self.mouse(event, io);
                }
            }
        }
    }
    pub(super) fn terminal_chunk(
        &mut self,
        bytes: &[u8],
        io: &Io,
        deferred: &mut std::collections::VecDeque<u8>,
    ) {
        let paste = self
            .terminal
            .as_ref()
            .is_some_and(|terminal| terminal.snapshot().bracketed_paste);
        let events = self.terminal_decoder.feed(bytes, paste);
        for event in events {
            self.terminal_event(event, io, deferred);
        }
        if !self.terminal_focused() {
            deferred.extend(self.terminal_decoder.take_pending());
            self.terminal_escape_at = None;
        }
    }
    pub(super) fn output_lines(&self, text: &str) -> Vec<String> {
        let width = self
            .displayed
            .as_ref()
            .and_then(|frame| frame.inspector_bounds())
            .map_or(72, |(_, _, width, _)| width)
            .max(1);
        let mut result = Vec::new();
        for line in text.lines() {
            let mut part = String::new();
            let mut used = 0;
            for glyph in crate::frontend::cells::graphemes(line) {
                let cells = crate::frontend::cells::width(glyph);
                if used + cells > width && !part.is_empty() {
                    result.push(std::mem::take(&mut part));
                    used = 0;
                }
                part.push_str(glyph);
                used += cells;
            }
            result.push(part);
        }
        result
    }
}

fn terminal_mouse(
    button: u16,
    release: bool,
    column: usize,
    row: usize,
    mode: crate::frontend::terminal_pane::MouseProtocolMode,
    encoding: crate::frontend::terminal_pane::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use crate::frontend::terminal_pane::{
        MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode,
    };
    if button > 223 || column == 0 || row == 0 || column > 65535 || row > 65535 {
        return None;
    }
    let motion = button & 32 != 0;
    if mode == Mode::None
        || mode == Mode::Press && (release || motion)
        || mode == Mode::PressRelease && motion
        || mode == Mode::ButtonMotion && motion && button & 3 == 3
    {
        return None;
    }
    if encoding == Encoding::Sgr {
        return Some(
            format!(
                "\x1b[<{button};{column};{row}{}",
                if release { 'm' } else { 'M' }
            )
            .into_bytes(),
        );
    }
    let code = (if release { 3 } else { button }).checked_add(32)?;
    if encoding == Encoding::Utf8 {
        let mut encoded = String::from("\x1b[M");
        for value in [
            usize::from(code),
            column.checked_add(32)?,
            row.checked_add(32)?,
        ] {
            encoded.push(char::from_u32(u32::try_from(value).ok()?)?);
        }
        Some(encoded.into_bytes())
    } else {
        Some(vec![
            27,
            b'[',
            b'M',
            u8::try_from(code).ok()?,
            u8::try_from(column.checked_add(32)?).ok()?,
            u8::try_from(row.checked_add(32)?).ok()?,
        ])
    }
}
#[cfg(test)]
mod tests {
    use super::terminal_mouse;
    use crate::frontend::terminal_pane::{
        MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode,
    };
    #[test]
    fn terminal_mouse_checks_protocol_and_encoding_bounds() {
        for button in [224, 255, 65535] {
            assert!(
                terminal_mouse(button, false, 1, 1, Mode::AnyMotion, Encoding::Default).is_none()
            );
        }
        assert!(terminal_mouse(32, false, 1, 1, Mode::PressRelease, Encoding::Sgr).is_none());
        assert!(terminal_mouse(0, true, 1, 1, Mode::Press, Encoding::Sgr).is_none());
        assert!(terminal_mouse(35, false, 1, 1, Mode::ButtonMotion, Encoding::Sgr).is_none());
        assert_eq!(
            terminal_mouse(0, false, 1, 1, Mode::Press, Encoding::Default),
            Some(vec![27, b'[', b'M', 32, 33, 33])
        );
        assert!(terminal_mouse(0, false, 224, 1, Mode::Press, Encoding::Default).is_none());
        assert!(
            std::str::from_utf8(
                &terminal_mouse(0, false, 256, 128, Mode::Press, Encoding::Utf8).unwrap()
            )
            .is_ok()
        );
        assert_eq!(
            terminal_mouse(52, false, 256, 128, Mode::ButtonMotion, Encoding::Sgr).unwrap(),
            b"\x1b[<52;256;128M"
        );
    }
}
