//! Legacy command-line grammar. Help is dispatched before option parsing.
use std::{collections::BTreeSet, path::PathBuf};
pub const HELP: &str = "Usage: ansa [--continue | --session ID] [--cwd PATH] [--model SLUG] [PROMPT]\n       ansa --openrouter [--continue | --session ID] [--cwd PATH]\n       ansa --openrouter-login [--auth-dir PATH] < KEY_FILE\n       ansa --offline [PROMPT]\n       ansa --login | --auth-status | --models | --logout\n       ansa --print-config\n       ansa --harness ACTION --cwd PROJECT [ARGS...]\n       ansa [--offline] --harness-trial PROPOSAL:ARM [--cwd PATH] [PROMPT]\n       ansa --export-evidence JOURNAL_DIR\nCommands: /status /cancel /resume /exit. Ctrl+C cancels; Ctrl+D finishes accepted work.\n";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub offline: bool,
    pub openrouter: bool,
    pub resume: bool,
    pub print_config: bool,
    pub harness_trial: Option<String>,
    pub cwd: PathBuf,
    pub auth: PathBuf,
    pub session: String,
    pub model: Option<String>,
    pub returned_model: Option<String>,
    pub prompt: Option<String>,
    pub workspace: Option<PathBuf>,
    pub workspace_editor: bool,
}
impl Options {
    pub fn parse(args: &[String], cwd: PathBuf, auth: PathBuf) -> super::Result<Self> {
        let mut value = Self {
            offline: false,
            openrouter: false,
            resume: false,
            print_config: false,
            harness_trial: None,
            cwd,
            auth,
            session: String::new(),
            model: None,
            returned_model: None,
            prompt: None,
            workspace: None,
            workspace_editor: false,
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
                "--openrouter" => value.openrouter = true,
                "--continue" => value.resume = true,
                "--print-config" => value.print_config = true,
                "--workspace-editor" => value.workspace_editor = true,
                "--native-terminals" => {} // Accepted for existing launch scripts.
                _ => {
                    let next = args.get(index).ok_or("cli_option_value")?;
                    index += 1;
                    match key.as_str() {
                        "--session" => value.session = next.clone(),
                        "--harness-trial" => {
                            crate::harness::trial_parts(next)?;
                            value.harness_trial = Some(next.clone())
                        }
                        "--cwd" => value.cwd = next.into(),
                        "--auth-dir" => value.auth = next.into(),
                        "--model" => value.model = Some(next.clone()),
                        "--returned-model" => value.returned_model = Some(next.clone()),
                        "--workspace-dir" => value.workspace = Some(next.into()),
                        _ => return Err("unknown_cli_option"),
                    }
                }
            }
        }
        if value.harness_trial.is_some()
            && (value.resume || !value.session.is_empty() || value.print_config)
        {
            return Err("harness_trial_requires_new_session");
        }
        if value.workspace_editor && value.workspace.is_none() {
            return Err("workspace_editor_requires_endpoint");
        }
        if value.openrouter && value.offline {
            return Err("provider_selection_conflict");
        }
        if value.prompt.as_deref().is_some_and(|s| {
            matches!(
                s,
                "/resume" | "/cancel" | "/status" | "/copy" | "/exit" | "/quit"
            )
        }) {
            return Err("slash_command_requires_stdin");
        }
        Ok(value)
    }
}
