//! Command discovery and completion for the chat composer.
use super::carbon::{Choice, Popup};
use super::input::Editor;
pub(crate) mod mention;

pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
}
const ALIASES: &[(&str, &str)] = &[
    ("/thinking", "/effort"),
    ("/resume", "/sessions"),
    ("/open", "/files"),
];

pub(super) fn canonical(name: &str) -> &str {
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, command)| command)
}

const COMMANDS: &[Command] = &[
    Command {
        name: "/model",
        description: "Choose a model and reasoning effort",
    },
    Command {
        name: "/effort",
        description: "Change reasoning effort",
    },
    Command {
        name: "/login",
        description: "Sign in with ChatGPT in your browser",
    },
    Command {
        name: "/theme",
        description: "Choose a theme",
    },
    Command {
        name: "/files",
        description: "Browse files, diffs, and the project terminal",
    },
    Command {
        name: "/agents",
        description: "View agents in this conversation",
    },
    Command {
        name: "/sessions",
        description: "Open a saved conversation, or search with /sessions search QUERY",
    },
    Command {
        name: "/retry",
        description: "Continue the paused request in this chat",
    },
    Command {
        name: "/new",
        description: "Start a new conversation",
    },
    Command {
        name: "/fork",
        description: "Fork the current conversation",
    },
    Command {
        name: "/btw",
        description: "Start a read-only side conversation",
    },
    Command {
        name: "/plan",
        description: "Toggle planning mode",
    },
    Command {
        name: "/copy",
        description: "Copy the confirmed transcript",
    },
    Command {
        name: "/accept",
        description: "Confirm manual checks for a completed plan",
    },
    Command {
        name: "/clarify",
        description: "Send an instruction within the approved plan",
    },
    Command {
        name: "/help",
        description: "Commands and shortcuts",
    },
    Command {
        name: "/exit",
        description: "Exit ANSA",
    },
    Command {
        name: "/autocompact",
        description: "Show or set the context compaction percentage",
    },
    Command {
        name: "/orchestrate",
        description: "Work with a lead agent that can delegate tasks",
    },
    Command {
        name: "/orchestrate+",
        description: "Configure role models or give the lead a task",
    },
    Command {
        name: "/propose",
        description: "Prepare a plan with explicit approval before execution",
    },
    Command {
        name: "/propose+",
        description: "Prepare an approval-based plan with configured role models",
    },
    Command {
        name: "/attach",
        description: "Attach a screenshot/image, or use --text PATH for a UTF-8 file",
    },
    Command {
        name: "/terminals",
        description: "View activity and the agent shell",
    },
];

pub fn available(native_terminals: bool) -> &'static [Command] {
    &COMMANDS[..COMMANDS.len() - usize::from(!native_terminals)]
}

pub(super) fn autocompact_percent(argument: &str) -> Result<Option<u8>, &'static str> {
    if argument.is_empty() {
        return Ok(None);
    }
    if argument.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(percent @ 1..=100) = argument.parse::<u8>()
    {
        return Ok(Some(percent));
    }
    Err("Use /autocompact with a percentage from 1 to 100")
}

pub(super) fn agent_task(text: &str) -> Option<&str> {
    let (verb, rest) = text.trim().split_once(char::is_whitespace)?;
    let (object, task) = rest
        .trim_start()
        .split_once(|ch: char| ch.is_whitespace() || ch == ':')?;
    if !matches!(
        (verb.to_lowercase().as_str(), object.to_lowercase().as_str()),
        ("запусти" | "попроси", "субагента" | "субагентов")
            | ("поручи", "субагенту" | "субагентам")
            | ("spawn" | "launch", "subagent" | "subagents")
    ) {
        return None;
    }
    let task = task.trim_start_matches(|ch: char| ch.is_whitespace() || ch == ':');
    (!task.trim().is_empty()).then_some(task.trim_end())
}

pub(super) fn skill_request(text: &str) -> Option<(&str, &str)> {
    let (name, task) = text
        .trim()
        .strip_prefix('$')?
        .split_once(char::is_whitespace)?;
    (!name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        && !task.trim().is_empty())
    .then(|| (name, task.trim()))
}

pub(super) struct SkillCommand {
    choice: Choice,
    body: Option<String>,
}

/// Derived from the catalogue attached to this conversation, never a fresh disk scan.
pub(super) fn skill_choices(profile: Option<&crate::harness::Pinned>) -> Vec<SkillCommand> {
    profile
        .into_iter()
        .flat_map(|p| p.complete(""))
        .filter_map(|(name, description)| {
            let label = format!("/{name}");
            let built_in = COMMANDS.iter().any(|cmd| cmd.name == canonical(&label))
                || matches!(
                    label.as_str(),
                    "/basic" | "/status" | "/cancel" | "/quit" | "/agent" | "/run" | "/send"
                );
            (!built_in).then(|| SkillCommand {
                choice: Choice {
                    label,
                    detail: description.into(),
                },
                body: profile
                    .and_then(|p| p.directory_body(name))
                    .map(str::to_owned),
            })
        })
        .collect()
}

pub(super) fn invoke_skill(
    skills: &[SkillCommand],
    command: &str,
    argument: &str,
) -> Option<String> {
    let skill = skills.iter().find(|skill| skill.choice.label == command)?;
    let name = skill.choice.label.strip_prefix('/')?;
    if let Some(body) = &skill.body {
        return Some(format!(
            "Use the pinned local skill `{name}` for this request within the current tool permissions.\nProcedure (quoted Markdown): {}\nUser task: {argument}",
            serde_json::json!(body)
        ));
    }
    Some(format!(
        "Use the pinned local skill `{name}` for this request. First call read_skill with name `{name}` and use the returned procedure within the current tool permissions.\nUser task: {argument}"
    ))
}

#[derive(Clone)]
pub struct Completion {
    pub begin: usize,
    pub end: usize,
    pub query: String,
    pub selected: usize,
    matches: Vec<Choice>,
}
impl Completion {
    pub fn exact_command(&self, editor: &Editor) -> bool {
        self.query.starts_with('/')
            && self.end == editor.cursor
            && self.matches.get(self.selected).is_some_and(|choice| {
                editor
                    .text()
                    .is_ok_and(|text| choice.label == canonical(text.trim()))
            })
    }
    pub fn at(editor: &Editor) -> Option<Self> {
        Self::at_with_commands(editor, available(false))
    }

    pub(crate) fn at_with_commands(editor: &Editor, commands: &[Command]) -> Option<Self> {
        Self::at_with_skills(editor, commands, &[])
    }

    pub(super) fn at_with_skills(
        editor: &Editor,
        commands: &[Command],
        skills: &[SkillCommand],
    ) -> Option<Self> {
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
        if matches!(
            text[..begin].split_whitespace().next(),
            Some("/orchestrate" | "/orchestrate+" | "/propose" | "/propose+" | "/attach")
        ) {
            return None;
        }
        let query = prefix.get(begin..)?;
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        let end = text[editor.cursor..]
            .find(char::is_whitespace)
            .map_or(text.len(), |offset| editor.cursor + offset);
        let mut matches: Vec<_> = commands
            .iter()
            .filter_map(|command| {
                let matches = command.name.starts_with(query)
                    || ALIASES
                        .iter()
                        .any(|(alias, target)| *target == command.name && alias.starts_with(query));
                matches.then(|| Choice {
                    label: command.name.to_owned(),
                    detail: command.description.to_owned(),
                })
            })
            .collect();
        if text[..begin].trim().is_empty() {
            matches.extend(
                skills
                    .iter()
                    .map(|skill| &skill.choice)
                    .filter(|skill| skill.label.starts_with(query))
                    .cloned(),
            );
        }
        if matches.is_empty() {
            return None;
        }
        Some(Self {
            begin,
            end,
            query: query.into(),
            selected: matches
                .iter()
                .position(|choice| choice.label == canonical(query))
                .unwrap_or(0),
            matches,
        })
    }
    pub(crate) fn paths(token: &mention::Mention<'_>, choices: Vec<Choice>) -> Self {
        Self {
            begin: token.begin,
            end: token.end,
            query: token.query.into(),
            selected: 0,
            matches: choices,
        }
    }
    pub(crate) fn loading(&self) -> bool {
        self.matches.is_empty()
    }
    pub(crate) fn directory_selected(&self) -> bool {
        self.query.starts_with('@')
            && self
                .matches
                .get(self.selected)
                .is_some_and(|choice| choice.label.ends_with('/'))
    }
    pub fn navigate(&mut self, older: bool) {
        self.selected = if older {
            self.selected.saturating_sub(1)
        } else {
            (self.selected + 1).min(self.matches.len().saturating_sub(1))
        };
    }
    pub fn apply(&self, editor: &mut Editor) -> bool {
        let Some(choice) = self.matches.get(self.selected) else {
            return false;
        };
        if self.query.starts_with('@') {
            let mut inserted = mention::insert(&self.query, &choice.label);
            if !self.directory_selected() && self.end == editor.bytes.len() {
                inserted.push(' ');
            }
            editor.replace(self.begin, self.end, inserted.as_bytes())
        } else {
            editor.replace(self.begin, self.end, choice.label.as_bytes())
        }
    }
    pub fn popup(&self, exact_command: bool) -> Popup {
        Popup {
            title: if self.query.starts_with('@') {
                "Files"
            } else {
                "Commands"
            }
            .into(),
            subtitle: String::new(),
            items: self.matches.clone(),
            selected: self.selected,
            footer: if self.loading() {
                "Reading directory…"
            } else if exact_command {
                "Enter run · Tab/→ complete · Esc close"
            } else {
                "↑/↓ select · Enter/Tab/→ complete · Esc close"
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
        title: "ANSA commands".into(),
        subtitle: "@ mention a file · Ctrl+F files · Ctrl+T agents · Ctrl+O tool details".into(),
        items: commands
            .iter()
            .map(|command| Choice {
                label: command.name.into(),
                detail: command.description.into(),
            })
            .collect(),
        selected,
        footer: "↑/↓ select · Enter insert · Esc close".into(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn autocompact_accepts_only_percentages_in_range() {
        assert_eq!(super::autocompact_percent(""), Ok(None));
        assert_eq!(super::autocompact_percent("1"), Ok(Some(1)));
        assert_eq!(super::autocompact_percent("100"), Ok(Some(100)));
        for invalid in ["0", "101", "256", "+10", "20%", "-1", "60 70"] {
            assert!(super::autocompact_percent(invalid).is_err());
        }
    }

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
    fn exact_skill_wins_over_a_longer_prefix_without_shadowing_a_builtin() {
        let skill = SkillCommand {
            choice: Choice {
                label: "/mod".into(),
                detail: "test".into(),
            },
            body: Some("Only this selected procedure".into()),
        };
        let input = editor("/mod");
        let skills = [skill];
        let completion = Completion::at_with_skills(&input, available(true), &skills).unwrap();
        assert!(completion.exact_command(&input));
        assert!(
            invoke_skill(&skills, "/mod", "preserve аргументы")
                .unwrap()
                .contains("Only this selected procedure")
        );
        assert!(invoke_skill(&skills, "/model", "").is_none());
        assert!(
            Completion::at_with_skills(&editor("path/mod"), available(true), &skills).is_none()
        );
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
        for (alias, canonical) in ALIASES {
            let input = editor(alias);
            let completion = Completion::at(&input).unwrap();
            assert!(completion.exact_command(&input));
            let popup = completion.popup(true);
            assert_eq!(popup.items.len(), 1);
            assert_eq!(popup.items[0].label, *canonical);
        }
        let input = editor("/term");
        assert!(Completion::at(&input).is_none());
        assert!(Completion::at_with_commands(&input, available(true)).is_some());
    }
}
