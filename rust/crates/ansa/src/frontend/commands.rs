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
        name: "/skills",
        description: "Browse skills available in this conversation",
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
];

pub fn available() -> &'static [Command] {
    COMMANDS
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

pub(super) type SkillCommand = Choice;

fn is_system_command(name: &str) -> bool {
    COMMANDS.iter().any(|cmd| cmd.name == canonical(name))
        || matches!(
            name,
            "/" | "?"
                | "/basic"
                | "/status"
                | "/cancel"
                | "/quit"
                | "/agent"
                | "/pr"
                | "/run"
                | "/send"
                | "/propose"
                | "/propose+"
                | "/clarify"
                | "/accept"
                | "/retry"
                | "/attach"
                | "/terminals"
        )
}

fn is_skill_prompt(text: &str) -> bool {
    !text
        .split_whitespace()
        .next()
        .is_some_and(is_system_command)
        && skill_request(text).is_none()
        && agent_task(text).is_none()
}

// Painting only needs the leading command context. Consult the rest of a
// long draft only when the bounded prefix cuts through that context.
pub(super) fn skill_prompt_prefix(prefix: &str, input: &[u8]) -> bool {
    if prefix.len() == input.len() {
        return is_skill_prompt(prefix);
    }
    let prefix = prefix.trim_start();
    if let Some((first, _)) = prefix.split_once(char::is_whitespace) {
        if is_system_command(first) {
            return false;
        }
        if !first.starts_with('$')
            && !matches!(
                first.to_lowercase().as_str(),
                "запусти" | "попроси" | "поручи" | "spawn" | "launch"
            )
        {
            return true;
        }
        if !is_skill_prompt(prefix) {
            return false;
        }
    } else if prefix.len() > 65 {
        // No built-in command or delegation name can fill this first word.
        return true;
    }
    std::str::from_utf8(input).is_ok_and(is_skill_prompt)
}

fn slash_boundary(text: &str, begin: usize) -> bool {
    let Some((at, previous)) = text[..begin].char_indices().next_back() else {
        return true;
    };
    previous.is_whitespace()
        && text[..at]
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'\\')
            .count()
            .is_multiple_of(2)
}

/// Derived from the catalogue attached to this conversation, never a fresh disk scan.
pub(super) fn skill_choices(profile: Option<&crate::harness::Pinned>) -> Vec<SkillCommand> {
    profile
        .into_iter()
        .flat_map(|p| p.complete(""))
        .filter_map(|(name, description)| {
            let label = format!("/{name}");
            (!is_system_command(&label)).then(|| Choice {
                label,
                detail: description.into(),
            })
        })
        .collect()
}

pub(super) fn skill_tokens<'a>(
    text: &'a str,
    skills: &'a [SkillCommand],
) -> impl Iterator<Item = (usize, &'a str)> {
    text.match_indices('/').filter_map(move |(begin, _)| {
        if !slash_boundary(text, begin) {
            return None;
        }
        let token = text[begin..].split_whitespace().next()?;
        // The pinned catalogue is sorted by name in harness::Pinned.
        skills
            .binary_search_by_key(&token, |skill| skill.label.as_str())
            .ok()?;
        Some((begin, token))
    })
}

pub(super) fn invoke_skills(skills: &[SkillCommand], text: &str) -> Option<String> {
    if skills.is_empty() || !is_skill_prompt(text) {
        return None;
    }
    let text = text.trim();
    let mut seen = std::collections::HashSet::new();
    let selected: Vec<_> = skill_tokens(text, skills)
        .filter(|(_, token)| seen.insert(*token))
        .collect();
    match selected.as_slice() {
        [] => None,
        &[(begin, token)] => {
            let name = &token[1..];
            let task = if begin == 0 {
                text[token.len()..].trim_start()
            } else {
                text
            };
            Some(format!(
                "Use the pinned local skill `{name}` for this request. First call read_skill with name `{name}` and use the returned procedure within the current tool permissions.\nUser task: {task}"
            ))
        }
        _ => {
            let names = selected
                .iter()
                .map(|(_, token)| format!("`{}`", &token[1..]))
                .collect::<Vec<_>>()
                .join(", ");
            Some(format!(
                "Use the pinned local skills {names} for this request. First call read_skill for each name in the listed order and use the returned procedures within the current tool permissions.\nUser task: {text}"
            ))
        }
    }
}

pub(super) fn accepts_file_mentions(text: &str, skills: &[SkillCommand]) -> bool {
    !text.trim_start().starts_with('/')
        || (!skills.is_empty()
            && is_skill_prompt(text)
            && skill_tokens(text, skills).next().is_some())
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
                if is_system_command(&choice.label) {
                    editor
                        .text()
                        .is_ok_and(|text| choice.label == canonical(text.trim()))
                } else {
                    choice.label == self.query
                }
            })
    }
    pub fn at(editor: &Editor) -> Option<Self> {
        Self::at_with_commands(editor, available())
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
        if !slash_boundary(text, begin) {
            return None;
        }
        let leading = text[..begin].trim().is_empty();
        if !leading && (skills.is_empty() || !is_skill_prompt(text)) {
            return None;
        }
        let query = prefix.get(begin..)?;
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        let end = text[editor.cursor..]
            .find(char::is_whitespace)
            .map_or(text.len(), |offset| editor.cursor + offset);
        if !text[begin + 1..end]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+'))
        {
            return None;
        }
        let mut matches: Vec<_> = commands
            .iter()
            .filter(|_| leading)
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
        matches.extend(
            skills
                .iter()
                .filter(|skill| skill.label.starts_with(query))
                .cloned(),
        );
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
            } else if self
                .matches
                .iter()
                .all(|choice| !is_system_command(&choice.label))
            {
                "Skills"
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
    help_with_commands(selected, available())
}

pub(super) fn skills(items: &[SkillCommand]) -> Popup {
    Popup {
        title: "Skills".into(),
        subtitle: if items.is_empty() {
            "No skills in this conversation"
        } else {
            ""
        }
        .into(),
        items: items.to_vec(),
        selected: 0,
        footer: if items.is_empty() {
            "Esc close · /new loads ~/.ansa/skills"
        } else {
            "↑/↓ select · Enter insert · Esc close"
        }
        .into(),
    }
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
    fn mid_message_completion_lists_skills_and_replaces_only_the_token() {
        let mut input = editor("Ж Проверь /ro потом файл");
        input.cursor = "Ж Проверь /ro".len();
        assert!(Completion::at(&input).is_none());
        let skills = [SkillCommand {
            label: "/route".into(),
            detail: "Choose a workflow".into(),
        }];
        let completion = Completion::at_with_skills(&input, available(), &skills).unwrap();
        assert!(completion.apply(&mut input));
        assert_eq!(input.text().unwrap(), "Ж Проверь /route потом файл");
        assert_eq!(input.cursor, "Ж Проверь /route".len());
        let mut path = editor("Ж Проверь /route/file");
        path.cursor = "Ж Проверь /ro".len();
        assert!(Completion::at_with_skills(&path, available(), &skills).is_none());
        assert!(Completion::at_with_skills(&editor("/help /ro"), available(), &skills).is_none());
    }
    #[test]
    fn exact_skill_wins_over_a_longer_prefix_without_shadowing_a_builtin() {
        let skill = SkillCommand {
            label: "/mod".into(),
            detail: "test".into(),
        };
        let input = editor("/mod");
        let skills = [skill];
        let completion = Completion::at_with_skills(&input, available(), &skills).unwrap();
        assert!(completion.exact_command(&input));
        assert!(
            invoke_skills(&skills, "/mod preserve аргументы")
                .unwrap()
                .contains("call read_skill with name `mod`")
        );
        assert!(invoke_skills(&skills, "/model /mod").is_none());
        assert!(Completion::at_with_skills(&editor("path/mod"), available(), &skills).is_none());
    }

    #[test]
    fn inline_skills_keep_order_and_skip_paths_and_escaped_mentions() {
        let skills = ["/grill-me", "/route"].map(|label| SkillCommand {
            label: label.into(),
            detail: String::new(),
        });
        let text = r"Проверь @some\ /route https://example.test/grill-me /route/dir /grill-me /route /grill-me";
        let prompt = invoke_skills(&skills, text).unwrap();
        assert!(
            prompt.starts_with("Use the pinned local skills `grill-me`, `route` for this request.")
        );
        assert!(prompt.ends_with(text));
        assert!(invoke_skills(&skills, r"@some\ /route \/grill-me /route.txt").is_none());
        assert!(invoke_skills(&skills, "Запусти субагента: проверь /route").is_none());
        let draft = editor("Проверь /grill-me");
        assert!(
            Completion::at_with_skills(&draft, available(), &skills)
                .unwrap()
                .exact_command(&draft)
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
    }

    #[test]
    fn contextual_commands_are_reserved_but_not_suggested() {
        for (name, prefix) in [
            ("/terminals", "/term"),
            ("/retry", "/ret"),
            ("/attach", "/att"),
        ] {
            assert!(is_system_command(name));
            assert!(Completion::at(&editor(prefix)).is_none());
            assert!(!help(0).items.iter().any(|item| item.label == name));
        }
    }
}
