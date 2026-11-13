use super::{AgentSettings, Result, path_text, read_text, s};
use crate::{
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::Epoch,
};
use std::path::{Path, PathBuf};
#[derive(Clone, Debug)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub text: String,
}
pub fn load_instructions(
    directory: &Path,
    home: &Path,
    mut limit: usize,
) -> Result<Vec<InstructionFile>> {
    let mut files = vec![];
    let mut add = |path: PathBuf| -> Result<()> {
        if let Some(text) = read_text(&path, limit)? {
            limit -= text.len();
            files.push(InstructionFile { path, text });
        }
        Ok(())
    };
    add(home.join(".ansa/AGENTS.md"))?;
    let directory = directory.canonicalize().map_err(|_| "file_access_failed")?;
    let parents: Vec<_> = directory.ancestors().collect();
    if parents.len() > 128 {
        return Err("instruction_depth_limit");
    }
    for parent in parents.iter().rev() {
        add(parent.join("AGENTS.md"))?;
    }
    Ok(files)
}
pub const BASE: &str = concat!(
    "You are ansa, a local general-purpose agent. Solve the user's task through the advertised tools. ",
    "The working directory is context, not a Git requirement or a filesystem sandbox. Use absolute paths or paths relative to it. ",
    "Make multiple tool rounds as needed. A process exit code or a tool error is information: inspect it and choose the next step. ",
    "Do not repeat operations with unknown effects. Sudo and privilege elevation through exec are disabled by the backend. ",
    "Do not ask for or handle sudo passwords. Do not read dotenv files or agent credential stores. ",
    "Respect applicable AGENTS.md instructions; directory-specific instructions are also returned by file tools. ",
    "Never infer configuration or permissions from AGENTS.md: configuration is validated separately. ",
    "Tools are non-interactive; never start background services as a substitute for a managed tool. ",
    "Keep user-facing answers concise and use the user's language."
);
pub fn saved_instructions(directory: &Path, home: &Path) -> Result<Vec<Json>> {
    load_instructions(directory, home, 64 * 1024)?
        .into_iter()
        .map(|file| {
            Json::object(vec![
                ("path", s(path_text(&file.path)?)?),
                ("text", s(&file.text)?),
            ])
        })
        .collect()
}
pub fn agent_epoch(settings: &AgentSettings) -> Result<Epoch> {
    let files = saved_instructions(&settings.cwd, &settings.home)?;
    let mut instructions = String::from(BASE);
    for file in &files {
        instructions.push_str(&format!(
            "\n\nInstructions from {}:\n{}",
            file.text_field("path")?,
            file.text_field("text")?
        ));
    }
    let files = Json::array(files)?;
    let token = sha256(files.dump().as_bytes());
    instructions.push_str(&format!("\nFor tools in the initial working directory the instruction_token is {token}. If a tool returns instructions_required, read the instructions and repeat with its instruction_token. Use file tools to discover directory-specific instructions before operating in other directories."));
    Ok(Epoch {
        id: "ansa_local_v1".into(),
        instructions,
        exact_state: Json::object(vec![
            ("settings", settings.effective()?),
            ("instructions", files),
            ("compaction", s("prompt_summary_keep_two_v1")?),
        ])?,
    })
}
