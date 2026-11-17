//! Bounded PNG previews for a native Carbon inspector.
use super::{Result, sessions::Factory, workspace::FileHost};
use crate::{
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::replay::{Effects, Execution},
    session::transcript::{Entry, ToolPhase},
    tools::artifacts::{PREVIEW_PNG_BYTES, read_preview_png, validate_descriptor},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    fmt::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone)]
pub(super) enum Source {
    File { path: PathBuf, hash: Option<String> },
    Artifact { session: String, descriptor: Json },
}

impl Source {
    pub fn from_entry<'a>(entry: &'a Entry, session: &str) -> Option<(&'a str, Self)> {
        let tool = entry.tool.as_ref()?;
        if entry.role != "tool_result"
            || tool.continued
            || !tool.last_chunk
            || !matches!(tool.name.as_str(), "show_png" | "python_execute")
            || tool.phase
                != (ToolPhase::Finished {
                    execution: Execution::Succeeded,
                    effects: Effects::KnownResult,
                })
        {
            return None;
        }
        let body =
            Json::parse_with_limits(entry.text.get(tool.content_start..)?, 128 * 1024, 16, 4096)
                .ok()?;
        let source = match (tool.name.as_str(), body.text_field("status").ok()?) {
            ("show_png", "png_requested") => Self::File {
                path: PathBuf::from(body.text_field("path").ok()?),
                hash: Some(body.text_field("sha256").ok()?.to_owned()),
            },
            ("python_execute", "ready") => {
                let items = body.field("artifacts").ok()?.elements().ok()?;
                if items.len() != 3
                    || items.iter().any(|item| {
                        validate_descriptor(item, session, Some(&tool.operation)).is_err()
                    })
                {
                    return None;
                }
                Self::Artifact {
                    session: session.into(),
                    descriptor: items
                        .iter()
                        .find(|item| item.text_field("mime") == Ok("image/png"))?
                        .clone(),
                }
            }
            _ => return None,
        };
        Some((&tool.operation, source))
    }

    pub fn name(&self) -> String {
        match self {
            Self::File { path, .. } => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned(),
            Self::Artifact { descriptor, .. } => descriptor
                .text_field("title")
                .unwrap_or("chart.png")
                .to_owned(),
        }
    }

    pub fn load(&self, files: &FileHost, factory: &Factory) -> Result<Image> {
        let (path, bytes) = match self {
            Self::File { path, hash } => {
                let path = files.validate_file(path)?;
                let bytes = read_preview_png(&path)?;
                if hash
                    .as_ref()
                    .is_some_and(|expected| sha256(&bytes) != *expected)
                {
                    return Err("preview_png_changed");
                }
                (path, bytes)
            }
            Self::Artifact {
                session,
                descriptor,
            } => {
                if descriptor.usize_field("size_bytes")? > PREVIEW_PNG_BYTES {
                    return Err("preview_png_size");
                }
                factory.artifacts(session)?.load(descriptor)?
            }
        };
        let dimension = |offset| {
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("validated PNG"))
        };
        Ok(Image {
            path,
            width: dimension(16),
            height: dimension(20),
            data: STANDARD.encode(bytes).into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Image {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    data: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Placement {
    pub id: u32,
    position: (usize, usize),
    columns: Option<usize>,
    rows: Option<usize>,
    data: Arc<str>,
}
impl Placement {
    pub fn new(
        id: u32,
        image: &Image,
        bounds: (usize, usize, usize, usize),
        cell: Option<(f64, f64)>,
    ) -> Self {
        let (col, row, width, height) = bounds;
        let (cell_width, cell_height) = cell.unwrap_or((1.0, 2.0));
        let aspect = f64::from(image.width) * cell_height / (f64::from(image.height) * cell_width);
        let (columns, rows) = if aspect * height as f64 >= width as f64 {
            (
                Some(width),
                cell.is_none()
                    .then(|| ((width as f64 / aspect).round() as usize).clamp(1, height)),
            )
        } else {
            (
                cell.is_none()
                    .then(|| ((height as f64 * aspect).round() as usize).clamp(1, width)),
                Some(height),
            )
        };
        Self {
            id,
            position: (col, row),
            columns,
            rows,
            data: image.data.clone(),
        }
    }

    pub fn paint(&self, into: &mut String) {
        let (col, row) = self.position;
        let _ = write!(into, "\x1b7\x1b[{};{}H", row + 1, col + 1);
        let mut chunks = self.data.as_bytes().chunks(4096).peekable();
        let mut first = true;
        while let Some(chunk) = chunks.next() {
            into.push_str("\x1b_G");
            if first {
                let _ = write!(into, "a=T,f=100,t=d,i={},C=1,q=2,", self.id);
                if let Some(columns) = self.columns {
                    let _ = write!(into, "c={columns},");
                }
                if let Some(rows) = self.rows {
                    let _ = write!(into, "r={rows},");
                }
                first = false;
            }
            let _ = write!(into, "m={};", usize::from(chunks.peek().is_some()));
            into.push_str(std::str::from_utf8(chunk).expect("base64 ASCII"));
            into.push_str("\x1b\\");
        }
        into.push_str("\x1b8");
    }
}

pub(super) fn delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2;\x1b\\")
}

pub(super) fn supported() -> bool {
    !["NO_COLOR", "TMUX", "SSH_TTY", "SSH_CONNECTION"]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
        && (matches!(
            std::env::var("TERM").as_deref(),
            Ok("xterm-kitty" | "xterm-ghostty")
        ) || std::env::var("TERM_PROGRAM").as_deref() == Ok("ghostty"))
}

pub(super) fn image_id() -> Option<u32> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut raw = [0u8; 4];
    SystemRandom::new().fill(&mut raw).ok()?;
    Some(u32::from_le_bytes(raw) % i32::MAX as u32 + 1)
}

pub(super) fn is_png(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
}
