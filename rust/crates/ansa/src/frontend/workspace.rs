//! Read-only workspace inspection and deterministic, journaled console requests.
//!
//! File jobs are blocking: the frontend submits them to its bounded blocking
//! worker and retains their cancellation token and join handle. Console commands
//! use a separate ordinary Runtime, so its durable intent and process supervisor
//! are the same ones used by model-requested tools.
use super::Result;
use crate::{
    auth::Cancel,
    config::{AgentSettings, n, s},
    process::git::{self, GitOutput},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{native, request::Request, responses::AdmittedResponse},
    runtime::Generate,
};
use std::{
    io::Read,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const FILE_BYTES: usize = 256 * 1024;
const DIFF_BYTES: usize = 128 * 1024;
const LIST_ENTRIES: usize = 256;
const GIT_TIME: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub directory: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileListing {
    pub path: PathBuf,
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffStatus {
    Unchanged,
    Modified,
    Untracked,
    NoRepository,
    NoCommit,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub status: DiffStatus,
    pub base: String,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDocument {
    pub path: PathBuf,
    /// Current file snapshot, with terminal controls represented as text.
    pub source: String,
    pub truncated: bool,
    pub diff: FileDiff,
    pub preview: Option<super::markdown::Document>,
}

#[derive(Clone, Debug)]
pub struct FileHost {
    cwd: PathBuf,
    protected: Vec<PathBuf>,
    exclude: Vec<String>,
}

impl FileHost {
    pub fn new(settings: &AgentSettings, auth: &Path) -> Result<Self> {
        if !settings.cwd.is_absolute() || !settings.directory.is_absolute() {
            return Err("workspace_directory");
        }
        let cwd = ansa_linux::fs::normalize(&settings.cwd);
        let mut protected = Vec::new();
        for path in [
            auth.to_path_buf(),
            settings.directory.join("auth"),
            settings.directory.join("sessions"),
        ] {
            // Opens reject every symlink component. Lexical protected roots
            // therefore suffice, and construction does not touch filesystem I/O.
            protected.push(ansa_linux::fs::normalize(
                &std::path::absolute(path).map_err(|_| "workspace_protected_path")?,
            ));
        }
        Ok(Self {
            cwd,
            protected,
            exclude: settings.exclude.clone(),
        })
    }

    fn blocked(&self, path: &Path) -> bool {
        ansa_linux::fs::protected_path(path)
            || self.protected.iter().any(|root| path.starts_with(root))
    }

    fn path(&self, path: &Path) -> Result<PathBuf> {
        let bytes = path.as_os_str().as_encoded_bytes();
        if bytes.is_empty() || bytes.len() > 4096 || bytes.contains(&0) {
            return Err("workspace_path");
        }
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        if self.blocked(&path) {
            return Err("protected_file");
        }
        let path = ansa_linux::fs::normalize(&path);
        if self.blocked(&path) {
            return Err("protected_file");
        }
        Ok(path)
    }

    /// Never follows a symlink component; a hard-linked file is also rejected.
    pub fn validate_file(&self, path: &Path) -> Result<PathBuf> {
        let path = self.path(path)?;
        let file = ansa_linux::fs::open_nofollow(&path, ansa_linux::local::read_flags())
            .map_err(|_| "workspace_file_access")?;
        if !ansa_linux::fs::regular_single(&file).map_err(|_| "workspace_file_type")? {
            return Err("workspace_file_type");
        }
        Ok(path)
    }

    pub fn list(&self, path: &Path, stop: &Cancel) -> Result<FileListing> {
        check_cancel(stop)?;
        let path = self.path(path)?;
        let directory = ansa_linux::fs::open_nofollow(&path, ansa_linux::local::read_flags())
            .map_err(|_| "workspace_directory")?;
        if !directory
            .metadata()
            .map_err(|_| "workspace_directory")?
            .is_dir()
        {
            return Err("workspace_directory");
        }
        // Enumerate the directory we opened, rather than resolving its pathname
        // a second time if an external writer renames a parent meanwhile.
        let held_directory = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let mut entries = Vec::new();
        let mut visited = 0usize;
        let mut truncated = false;
        for entry in std::fs::read_dir(held_directory).map_err(|_| "workspace_directory")? {
            check_cancel(stop)?;
            visited += 1;
            if visited > 4096 || entries.len() == LIST_ENTRIES {
                truncated = true;
                break;
            }
            let entry = entry.map_err(|_| "workspace_directory")?;
            let name = entry.file_name();
            let original = path.join(&name);
            if self.blocked(&original)
                || self
                    .exclude
                    .iter()
                    .any(|exclude| name.as_encoded_bytes() == exclude.as_bytes())
            {
                continue;
            }
            let kind = entry.file_type().map_err(|_| "workspace_file_type")?;
            if kind.is_symlink() || !(kind.is_file() || kind.is_dir()) {
                continue;
            }
            if kind.is_file()
                && entry
                    .metadata()
                    .is_ok_and(|meta| std::os::unix::fs::MetadataExt::nlink(&meta) != 1)
            {
                continue;
            }
            entries.push(FileEntry {
                path: original,
                name: display_text(&ansa_linux::fs::utf8_text(name.as_encoded_bytes())),
                directory: kind.is_dir(),
            });
        }
        entries.sort_by(|a, b| b.directory.cmp(&a.directory).then(a.name.cmp(&b.name)));
        Ok(FileListing {
            path,
            entries,
            truncated,
        })
    }

    pub fn open(&self, path: &Path, stop: &Cancel) -> Result<FileDocument> {
        check_cancel(stop)?;
        let path = self.path(path)?;
        let file = ansa_linux::fs::open_nofollow(&path, ansa_linux::local::read_flags())
            .map_err(|_| "workspace_file_access")?;
        if !ansa_linux::fs::regular_single(&file).map_err(|_| "workspace_file_type")? {
            return Err("workspace_file_type");
        }
        let mut bytes = Vec::new();
        file.take(FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "workspace_file_read")?;
        check_cancel(stop)?;
        let truncated = bytes.len() > FILE_BYTES;
        bytes.truncate(FILE_BYTES);
        if bytes.contains(&0) {
            return Err("workspace_binary_file");
        }
        let source = ansa_linux::fs::utf8_text(&bytes);
        let diff = match self.diff(&path, &source, truncated, stop) {
            Ok(diff) => diff,
            Err("workspace_cancelled") => return Err("workspace_cancelled"),
            Err(_) => FileDiff {
                status: DiffStatus::Unavailable,
                base: "HEAD".into(),
                text: String::new(),
                truncated: false,
            },
        };
        let preview = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
            .then(|| markdown_preview(&source));
        check_cancel(stop)?;
        Ok(FileDocument {
            path,
            source: display_text(&source),
            truncated,
            diff,
            preview,
        })
    }

    fn diff(&self, path: &Path, source: &str, truncated: bool, stop: &Cancel) -> Result<FileDiff> {
        let unavailable = |status| FileDiff {
            status,
            base: "HEAD".into(),
            text: String::new(),
            truncated: false,
        };
        if truncated {
            return Ok(unavailable(DiffStatus::Unavailable));
        }
        let root = match git(&self.cwd, &["rev-parse", "--show-toplevel"], stop)? {
            GitOutput::Success(bytes) => PathBuf::from(
                std::str::from_utf8(&bytes)
                    .map_err(|_| "workspace_git_path")?
                    .trim_end_matches('\n'),
            ),
            GitOutput::Failure => return Ok(unavailable(DiffStatus::NoRepository)),
            GitOutput::Missing | GitOutput::TooLarge => {
                return Ok(unavailable(DiffStatus::Unavailable));
            }
        };
        let Ok(relative) = path.strip_prefix(&root) else {
            return Ok(unavailable(DiffStatus::NoRepository));
        };
        let relative = relative.to_str().ok_or("workspace_git_path")?;
        if !matches!(
            git(&root, &["cat-file", "-e", "HEAD"], stop)?,
            GitOutput::Success(_)
        ) {
            return Ok(unavailable(DiffStatus::NoCommit));
        }
        let object = format!("HEAD:{relative}");
        let (base, status) = match git(&root, &["cat-file", "blob", &object], stop)? {
            GitOutput::Success(bytes) => {
                if bytes.contains(&0) {
                    return Ok(unavailable(DiffStatus::Unavailable));
                }
                (ansa_linux::fs::utf8_text(&bytes), DiffStatus::Modified)
            }
            GitOutput::Failure => {
                // A missing object in a partial clone is not an untracked file.
                // ls-tree inspects the committed tree without materializing it.
                match git(&root, &["ls-tree", "-z", "HEAD", "--", relative], stop)? {
                    GitOutput::Success(entries) if entries.is_empty() => {
                        (String::new(), DiffStatus::Untracked)
                    }
                    _ => return Ok(unavailable(DiffStatus::Unavailable)),
                }
            }
            GitOutput::Missing | GitOutput::TooLarge => {
                return Ok(unavailable(DiffStatus::Unavailable));
            }
        };
        if base == source {
            return Ok(unavailable(DiffStatus::Unchanged));
        }
        let (text, truncated) = unified_diff(relative, &base, source);
        Ok(FileDiff {
            status,
            base: "HEAD".into(),
            text,
            truncated,
        })
    }
}

fn check_cancel(stop: &Cancel) -> Result<()> {
    if stop.cancelled() {
        Err("workspace_cancelled")
    } else {
        Ok(())
    }
}

/// Represent terminal controls literally; no input can create CSI/OSC output.
pub fn display_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if (ch.is_control() && !matches!(ch, '\n' | '\t'))
            || matches!(ch as u32, 0x202a..=0x202e | 0x2066..=0x2069)
        {
            use std::fmt::Write;
            let _ = write!(out, "\\u{{{:x}}}", ch as u32);
        } else {
            out.push(ch);
        }
    }
    out
}

fn git(cwd: &Path, args: &[&str], stop: &Cancel) -> Result<GitOutput> {
    git::run(cwd, args, None, FILE_BYTES, GIT_TIME, stop)
}

fn unified_diff(path: &str, before: &str, after: &str) -> (String, bool) {
    let old: Vec<_> = before.lines().collect();
    let new: Vec<_> = after.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let start = prefix.saturating_sub(3);
    let old_end = (old.len() - suffix + 3).min(old.len());
    let new_end = (new.len() - suffix + 3).min(new.len());
    let mut output = format!(
        "--- HEAD/{path}\n+++ worktree/{path}\n@@ -{},{} +{},{} @@\n",
        if old.is_empty() { 0 } else { start + 1 },
        old_end - start,
        if new.is_empty() { 0 } else { start + 1 },
        new_end - start,
    );
    for (prefix, line) in old[start..prefix]
        .iter()
        .map(|line| (' ', *line))
        .chain(
            old[prefix..old.len() - suffix]
                .iter()
                .map(|line| ('-', *line)),
        )
        .chain(
            new[prefix..new.len() - suffix]
                .iter()
                .map(|line| ('+', *line)),
        )
        .chain(
            old[old.len() - suffix..old_end]
                .iter()
                .map(|line| (' ', *line)),
        )
    {
        let line = display_text(line);
        if output.len() + line.len() + 2 > DIFF_BYTES {
            return (output, true);
        }
        output.push(prefix);
        output.push_str(&line);
        output.push('\n');
    }
    if before.ends_with('\n') != after.ends_with('\n') {
        output.push_str("\\ Final newline changed\n");
    }
    (output, false)
}

pub fn markdown_preview(source: &str) -> super::markdown::Document {
    super::markdown::Document::parse(source)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorCommand {
    pub argv: Vec<String>,
}
impl EditorCommand {
    /// Parse EDITOR as argv, with quotes/backslashes but without shell expansion.
    /// The caller validates the path through FileHost immediately before launch
    /// and owns terminal suspension, the idle gate, process wait and repaint.
    pub fn parse(editor: &str, path: &Path) -> Result<Self> {
        if !path.is_absolute()
            || ansa_linux::fs::protected_path(path)
            || path.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err("workspace_editor_path");
        }
        let mut argv = split_argv(editor)?;
        argv.push("--".into());
        argv.push(path.to_str().ok_or("workspace_editor_path")?.into());
        Ok(Self { argv })
    }
}

fn split_argv(command: &str) -> Result<Vec<String>> {
    if command.len() > 4096 || command.contains('\0') {
        return Err("workspace_editor_command");
    }
    let mut result = Vec::new();
    let mut argument = String::new();
    let mut active = false;
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            argument.push(ch);
            escaped = false;
            active = true;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
            active = true;
        } else if let Some(expected) = quote {
            if ch == expected {
                quote = None;
            } else {
                argument.push(ch);
            }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
            active = true;
        } else if ch.is_whitespace() {
            if active {
                result.push(std::mem::take(&mut argument));
                active = false;
            }
        } else {
            argument.push(ch);
            active = true;
        }
    }
    if escaped || quote.is_some() {
        return Err("workspace_editor_command");
    }
    if active {
        result.push(argument);
    }
    if result.first().is_none_or(String::is_empty) || result.len() > 64 {
        return Err("workspace_editor_command");
    }
    Ok(result)
}

/// Build a provider adapter for a dedicated console Runtime. It never contacts
/// a model: the user's command is admitted as one ordinary run_command call.
/// The caller uses local_tools, a distinct session/journal, Basic mode and the
/// configured tool limits. Results enter the main conversation only explicitly.
pub fn console_generate(settings: &AgentSettings, shell: PathBuf) -> Result<Generate> {
    if !shell.is_absolute() || shell.as_os_str().as_encoded_bytes().contains(&0) {
        return Err("console_shell");
    }
    let shell = shell.to_str().ok_or("console_shell")?.to_string();
    let cwd = settings
        .cwd
        .to_str()
        .ok_or("console_directory")?
        .to_string();
    let timeout = settings.process_timeout_ms;
    Ok(Arc::new(move |request, stop, _| {
        console_response(request, stop, &cwd, &shell, timeout)
    }))
}

fn console_response(
    request: &Request,
    stop: &Cancel,
    cwd: &str,
    shell: &str,
    timeout: usize,
) -> Result<AdmittedResponse> {
    check_cancel(stop)?;
    let user = request
        .input
        .iter()
        .rposition(|item| item.text_field("role") == Ok("user"))
        .ok_or("console_command")?;
    let command = request.input[user].text_field("content")?;
    if command.trim().is_empty() || command.len() > 12 * 1024 || command.contains('\0') {
        return Err("console_command");
    }
    let suffix = &request.input[user + 1..];
    let digest = sha256(request.save()?.dump().as_bytes());
    let id = &digest[..24];
    let mut token = sha256(
        request
            .epoch
            .exact_state
            .field("instructions")?
            .dump()
            .as_bytes(),
    );
    let item = if let Some(last) = suffix.last() {
        if last.text_field("type")? != "function_call_output" {
            return Err("console_continuation");
        }
        let previous = suffix
            .iter()
            .rev()
            .skip(1)
            .find(|item| item.text_field("type") == Ok("function_call"))
            .ok_or("console_continuation")?;
        if previous.text_field("name")? != "run_command"
            || previous.text_field("namespace")? != "omh"
            || previous.text_field("call_id")? != last.text_field("call_id")?
        {
            return Err("console_continuation");
        }
        let output = last.text_field("output")?;
        let parsed = Json::parse_with_limits(output, 128 * 1024, 64, 131072)?;
        if parsed.text_field("status") == Ok("instructions_required")
            && parsed.field("executed")? == &Json::parse("false")?
        {
            token = parsed.text_field("instruction_token")?.into();
            let previous_args = Json::parse(previous.text_field("arguments")?)?;
            if previous_args.text_field("instruction_token")? == token || suffix.len() > 8 {
                return Err("console_instruction_conflict");
            }
            console_call(id, command, cwd, shell, timeout, &token)?
        } else {
            let mut text = String::new();
            for name in ["stdout", "stderr"] {
                if let Ok(value) = parsed.text_field(name) {
                    text.push_str(value);
                    if !value.is_empty() && !value.ends_with('\n') {
                        text.push('\n');
                    }
                }
            }
            if parsed.has("exit_code") {
                text.push_str(&format!("exit: {}", parsed.field("exit_code")?.dump()));
            } else {
                text.push_str(output);
            }
            console_message(id, &text)?
        }
    } else {
        console_call(id, command, cwd, shell, timeout, &token)?
    };
    native::load(&Json::object(vec![
        ("request", request.save()?),
        ("id", s(&format!("console_response_{id}"))?),
        ("model", s(&request.binding.returned_model)?),
        ("items", Json::array(vec![item])?),
    ])?)
}

fn console_call(
    id: &str,
    command: &str,
    cwd: &str,
    shell: &str,
    timeout: usize,
    token: &str,
) -> Result<Json> {
    let arguments = Json::object(vec![
        ("cwd", s(cwd)?),
        ("argv", Json::array(vec![s(shell)?, s("-c")?, s(command)?])?),
        ("timeout_ms", n(timeout)),
        ("instruction_token", s(token)?),
    ])?;
    Json::object(vec![
        ("type", s("function_call")?),
        ("id", s(&format!("console_item_{id}"))?),
        ("call_id", s(&format!("console_call_{id}"))?),
        ("namespace", s("omh")?),
        ("name", s("run_command")?),
        ("arguments", s(&arguments.dump())?),
        ("status", s("completed")?),
    ])
}

fn console_message(id: &str, text: &str) -> Result<Json> {
    Json::object(vec![
        ("type", s("message")?),
        ("id", s(&format!("console_message_{id}"))?),
        ("role", s("assistant")?),
        ("status", s("completed")?),
        (
            "content",
            Json::array(vec![Json::object(vec![
                ("type", s("output_text")?),
                ("text", s(text)?),
            ])?])?,
        ),
    ])
}
