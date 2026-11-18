//! Command discovery and completion for the chat composer.
use super::carbon::{Choice, Popup};
use super::input::Editor;

pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
}
const COMMANDS: &[Command] = &[
    Command {
        name: "/model",
        description: "Выбрать модель и уровень рассуждения",
    },
    Command {
        name: "/effort",
        description: "Изменить уровень рассуждения модели",
    },
    Command {
        name: "/thinking",
        description: "Уровень рассуждения · алиас /effort",
    },
    Command {
        name: "/theme",
        description: "Оформление интерфейса",
    },
    Command {
        name: "/files",
        description: "Файлы, diff и терминал проекта",
    },
    Command {
        name: "/agents",
        description: "Помощники текущего разговора",
    },
    Command {
        name: "/sessions",
        description: "Открыть сохранённый разговор",
    },
    Command {
        name: "/new",
        description: "Начать новый разговор",
    },
    Command {
        name: "/fork",
        description: "Продолжить разговор отдельной веткой",
    },
    Command {
        name: "/btw",
        description: "Побочный разговор с доступом только для чтения",
    },
    Command {
        name: "/plan",
        description: "Переключить режим планирования",
    },
    Command {
        name: "/copy",
        description: "Скопировать подтверждённый диалог",
    },
    Command {
        name: "/resume",
        description: "Открыть список сессий",
    },
    Command {
        name: "/help",
        description: "Команды и клавиши",
    },
    Command {
        name: "/exit",
        description: "Завершить ANSA",
    },
    Command {
        name: "/terminals",
        description: "Посмотреть терминал агента",
    },
];

pub fn available(native_terminals: bool) -> &'static [Command] {
    &COMMANDS[..COMMANDS.len() - usize::from(!native_terminals)]
}

pub(super) fn agent_task(text: &str) -> Option<&str> {
    let (verb, rest) = text.trim().split_once(char::is_whitespace)?;
    let (object, task) = rest
        .trim_start()
        .split_once(|ch: char| ch.is_whitespace() || ch == ':')?;
    if !matches!(
        (verb.to_lowercase().as_str(), object.to_lowercase().as_str()),
        ("запусти" | "попроси", "субагента") | ("поручи", "субагенту")
    ) {
        return None;
    }
    let task = task.trim_start_matches(|ch: char| ch.is_whitespace() || ch == ':');
    (!task.trim().is_empty()).then_some(task.trim_end())
}

#[derive(Clone)]
pub struct Completion {
    pub begin: usize,
    pub end: usize,
    pub query: String,
    pub selected: usize,
    matches: Vec<usize>,
}
impl Completion {
    pub fn exact_command(&self, editor: &Editor) -> bool {
        self.end == editor.cursor
            && self.matches.get(self.selected).is_some_and(|&index| {
                editor
                    .text()
                    .is_ok_and(|text| COMMANDS[index].name == text.trim())
            })
    }
    pub fn at(editor: &Editor) -> Option<Self> {
        Self::at_with_commands(editor, available(false))
    }

    pub(crate) fn at_with_commands(editor: &Editor, commands: &[Command]) -> Option<Self> {
        if editor.pasting() || editor.overflow {
            return None;
        }
        let text = editor.text().ok()?;
        let prefix = text.get(..editor.cursor)?;
        let begin = prefix.rfind('/')?;
        // A slash inside a path/URL is ordinary text. A command token may
        // occur at any word boundary, including in the middle of a message.
        if begin > 0 && !text[..begin].chars().next_back()?.is_whitespace() {
            return None;
        }
        let query = prefix.get(begin..)?;
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        let end = text[editor.cursor..]
            .find(char::is_whitespace)
            .map_or(text.len(), |offset| editor.cursor + offset);
        let matches: Vec<_> = commands
            .iter()
            .enumerate()
            .filter_map(|(i, command)| command.name.starts_with(query).then_some(i))
            .collect();
        if matches.is_empty() {
            return None;
        }
        Some(Self {
            begin,
            end,
            query: query.into(),
            selected: 0,
            matches,
        })
    }
    pub fn navigate(&mut self, older: bool) {
        self.selected = if older {
            self.selected.saturating_sub(1)
        } else {
            (self.selected + 1).min(self.matches.len().saturating_sub(1))
        };
    }
    pub fn apply(&self, editor: &mut Editor) -> bool {
        let Some(command) = self.matches.get(self.selected).map(|&i| &COMMANDS[i]) else {
            return false;
        };
        editor.replace(self.begin, self.end, command.name.as_bytes())
    }
    pub fn popup(&self, exact_command: bool) -> Popup {
        Popup {
            title: "Команды".into(),
            subtitle: String::new(),
            items: self
                .matches
                .iter()
                .map(|&i| Choice {
                    label: COMMANDS[i].name.into(),
                    detail: COMMANDS[i].description.into(),
                })
                .collect(),
            selected: self.selected,
            footer: if exact_command {
                "Enter выполнить · Tab/→ дописать · Esc закрыть"
            } else {
                "↑/↓ выбрать · Enter/Tab/→ дописать · Esc закрыть"
            }
            .into(),
        }
    }
}
pub fn help(selected: usize) -> Popup {
    help_with_commands(selected, available(false))
}

pub(crate) fn help_with_commands(selected: usize, commands: &[Command]) -> Popup {
    Popup {
        title: "Команды ANSA".into(),
        subtitle: "Ctrl+J новая строка · Ctrl+C очистить ввод или остановить шаг".into(),
        items: commands
            .iter()
            .map(|command| Choice {
                label: command.name.into(),
                detail: command.description.into(),
            })
            .collect(),
        selected,
        footer: "↑/↓ выбрать · Enter вставить · Esc закрыть".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn editor(text: &str) -> Editor {
        let mut editor = Editor::default();
        for &byte in text.as_bytes() {
            editor.feed(byte, true);
        }
        editor
    }
    #[test]
    fn mid_message_completion_replaces_only_the_token() {
        let mut input = editor("Ж Проверь /mo потом файл");
        input.cursor = "Ж Проверь /mo".len();
        let completion = Completion::at(&input).unwrap();
        assert!(completion.apply(&mut input));
        assert_eq!(input.text().unwrap(), "Ж Проверь /model потом файл");
        assert_eq!(input.cursor, "Ж Проверь /model".len());
    }
    #[test]
    fn paths_urls_and_paste_are_not_commands() {
        for text in [
            "https://example/model",
            "/home/user",
            "слово/model",
            "read /mo rest",
        ] {
            assert!(Completion::at(&editor(text)).is_none(), "{text}");
        }
        let mut input = editor("/");
        for &byte in b"\x1b[200~" {
            input.feed(byte, true);
        }
        assert!(Completion::at(&input).is_none());
    }
    #[test]
    fn navigation_changes_the_inserted_command_without_executing_it() {
        let mut input = editor("/");
        let mut completion = Completion::at(&input).unwrap();
        completion.navigate(false);
        assert!(completion.apply(&mut input));
        assert_eq!(input.text().unwrap(), "/effort");
        assert!(!COMMANDS.iter().any(|command| command.name == "/run"));
        let input = editor("/term");
        assert!(Completion::at(&input).is_none());
        assert!(Completion::at_with_commands(&input, available(true)).is_some());
    }
}
