//! Legacy command-line grammar. Help is dispatched before option parsing.
use std::{collections::BTreeSet, path::PathBuf};
pub const HELP: &str = "Usage: ansa [--continue | --session ID] [--cwd PATH] [--model SLUG] [PROMPT]\n       ansa --offline [PROMPT]\n       ansa --login | --auth-status | --models | --logout\n       ansa --print-config\nCommands: /status /cancel /resume /exit. Ctrl+C cancels; Ctrl+D finishes accepted work.\n";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub offline: bool,
    pub resume: bool,
    pub print_config: bool,
    pub cwd: PathBuf,
    pub auth: PathBuf,
    pub session: String,
    pub model: Option<String>,
    pub returned_model: Option<String>,
    pub prompt: Option<String>,
}
impl Options {
    pub fn parse(args: &[String], cwd: PathBuf, auth: PathBuf) -> super::Result<Self> {
        let mut value = Self {
            offline: false,
            resume: false,
            print_config: false,
            cwd,
            auth,
            session: String::new(),
            model: None,
            returned_model: None,
            prompt: None,
        };
        let mut seen = BTreeSet::new();
        let mut index = 0;
        while index < args.len() {
            let key = &args[index];
            index += 1;
            if !key.starts_with('-') {
                if value.prompt.replace(key.clone()).is_some() {
                    return Err("cli_prompt_arguments");
                }
                continue;
            }
            if !seen.insert(key) {
                return Err("duplicate_cli_option");
            }
            match key.as_str() {
                "--offline" => value.offline = true,
                "--continue" => value.resume = true,
                "--print-config" => value.print_config = true,
                _ => {
                    let next = args.get(index).ok_or("cli_option_value")?;
                    index += 1;
                    match key.as_str() {
                        "--session" => value.session = next.clone(),
                        "--cwd" => value.cwd = next.into(),
                        "--auth-dir" => value.auth = next.into(),
                        "--model" => value.model = Some(next.clone()),
                        "--returned-model" => value.returned_model = Some(next.clone()),
                        _ => return Err("unknown_cli_option"),
                    }
                }
            }
        }
        if value
            .prompt
            .as_deref()
            .is_some_and(|s| matches!(s, "/resume" | "/cancel" | "/status" | "/exit" | "/quit"))
        {
            return Err("slash_command_requires_stdin");
        }
        Ok(value)
    }
}
