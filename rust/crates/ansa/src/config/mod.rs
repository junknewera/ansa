pub mod agents;
pub mod settings;
pub use agents::{InstructionFile, agent_epoch, load_instructions};
pub use settings::{AgentSettings, load_agent_settings};
pub type Result<T> = std::result::Result<T, &'static str>;
pub(crate) fn n(value: usize) -> crate::protocol::json::Json {
    crate::protocol::json::Json::parse(&value.to_string()).expect("usize JSON")
}
pub(crate) fn s(value: &str) -> Result<crate::protocol::json::Json> {
    crate::protocol::json::Json::string(value)
}
pub(crate) fn path_text(path: &std::path::Path) -> Result<&str> {
    path.to_str().ok_or("json_string_invalid")
}
pub(crate) fn read_text(path: &std::path::Path, limit: usize) -> Result<Option<String>> {
    use std::io::Read;
    if ansa_linux::fs::protected_path(path) {
        return Err("protected_instruction_path");
    }
    let mut file = match ansa_linux::fs::open_nofollow(path, rustix_flags()) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("config_file_type"),
    };
    if !ansa_linux::fs::regular_single(&file).map_err(|_| "config_file_type")? {
        return Err("config_file_type");
    }
    let mut bytes = vec![];
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "config_file_read")?;
    if bytes.len() > limit {
        return Err("instruction_size_limit");
    }
    Ok(Some(String::from_utf8(bytes).map_err(|_| "invalid_utf8")?))
}
fn rustix_flags() -> ansa_linux::local::ReadFlags {
    ansa_linux::local::read_flags()
}
