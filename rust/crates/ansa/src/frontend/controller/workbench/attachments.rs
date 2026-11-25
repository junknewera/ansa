//! Snapshot a user-selected file once, off the Carbon input loop.
use super::*;
use crate::provider::contracts::MAX_IMAGE_BYTES;
use crate::provider::contracts::image::MAX_ORIGINAL_IMAGE_BYTES;
use std::{fs::File, io::Read, path::Component};

mod clipboard;

const INLINE_TEXT_BYTES: usize = 8 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const USAGE: &str =
    "Use /attach PATH (PNG/JPEG), /attach --text PATH (local text), or /attach --clear";

pub(super) enum Attachment {
    Image(ImageInput),
    Text(TextAttachment),
    ClipboardText(String),
}

fn parse_source(argument: &str) -> Option<(bool, &Path)> {
    let option = argument.split_whitespace().next()?;
    let (text, path) = if option == "--text" {
        (true, argument[option.len()..].trim_start())
    } else if option.starts_with("--") {
        return None;
    } else {
        (false, argument)
    };
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.chars().any(char::is_control) {
        return None;
    }
    Some((text, Path::new(path)))
}

fn protected(path: &Path) -> bool {
    ansa_linux::fs::protected_path(path)
        || path.components().any(|part| {
            let Component::Normal(name) = part else {
                return false;
            };
            let name = name.to_string_lossy().to_ascii_lowercase();
            let secret_name = name.strip_prefix('.').unwrap_or(&name);
            matches!(secret_name.split('.').next(), Some("secret" | "secrets"))
                || matches!(name.as_str(), ".env" | ".envrc")
                || name.starts_with(".env.")
                || name.ends_with(".env")
        })
}

fn open_selected(cwd: &Path, state: &Path, auth: &Path, path: &Path, text: bool) -> Result<File> {
    let path = cwd.join(path);
    // Don't normalize away components that may be symlinks. Text is limited to
    // the workspace; image attachments can still select an existing screenshot elsewhere.
    if path.components().any(|part| part == Component::ParentDir) {
        return Err("attachment_path");
    }
    if text && !path.starts_with(cwd) {
        return Err("attachment_outside_workspace");
    }
    if protected(&path) || path.starts_with(state) || path.starts_with(auth) {
        return Err("protected_file");
    }
    let file = ansa_linux::fs::open_nofollow(&path, ansa_linux::local::read_flags())
        .map_err(|_| "attachment_open")?;
    if !ansa_linux::fs::regular_single(&file).map_err(|_| "attachment_stat")? {
        return Err("attachment_regular_file_required");
    }
    Ok(file)
}

fn read_bounded(file: File, limit: usize, size_error: &'static str) -> Result<Vec<u8>> {
    let metadata = file.metadata().map_err(|_| "attachment_stat")?;
    if metadata.len() > limit as u64 {
        return Err(size_error);
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "attachment_read")?;
    if bytes.len() > limit {
        return Err(size_error);
    }
    Ok(bytes)
}

fn read_text(file: File) -> Result<String> {
    let bytes = read_bounded(file, INLINE_TEXT_BYTES, "attachment_text_size")?;
    let text = String::from_utf8(bytes).map_err(|_| "attachment_text_encoding")?;
    if text.trim().is_empty()
        || text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\r' | '\n' | '\t'))
    {
        return Err("attachment_text_encoding");
    }
    Ok(text)
}

pub(super) fn read_open_image(
    file: File,
    remaining: usize,
    original_remaining: usize,
) -> Result<ImageInput> {
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err("image_regular_file_required");
    }
    let bytes = read_bounded(file, original_remaining, "image_original_bytes")?;
    ImageInput::fit(&bytes, remaining)
}

fn load_attachment(
    cwd: &Path,
    state: &Path,
    auth: &Path,
    path: &Path,
    text: bool,
    remaining: usize,
    original_remaining: usize,
) -> Result<Attachment> {
    let file = open_selected(cwd, state, auth, path, text)?;
    if text {
        let attachment = match read_text(file) {
            Ok(text) => TextAttachment::Inline(text),
            Err("attachment_text_size") => {
                let absolute = cwd.join(path);
                let relative = absolute
                    .strip_prefix(cwd)
                    .map_err(|_| "attachment_outside_workspace")?;
                TextAttachment::File(relative.to_str().ok_or("attachment_path")?.into())
            }
            Err(code) => return Err(code),
        };
        Ok(Attachment::Text(attachment))
    } else {
        read_open_image(file, remaining, original_remaining).map(Attachment::Image)
    }
}

impl Slot {
    pub(super) fn attachment_loaded(
        &mut self,
        result: Result<Attachment>,
        from_clipboard: bool,
        io: &Io,
    ) {
        self.state.attachment_loading = false;
        let anchor = from_clipboard
            .then(|| self.state.editor.clipboard_anchor.take())
            .flatten();
        if from_clipboard && anchor.is_none() {
            return;
        }
        match result {
            Ok(Attachment::Text(text)) => {
                let status = match &text {
                    TextAttachment::Inline(_) => {
                        "Text attached · contents saved with the next Work/Plan message"
                    }
                    TextAttachment::File(_) => {
                        "Text attached · agent will read the local file as needed"
                    }
                };
                self.state.text_attachment = Some(text);
                self.state.report(io, status);
            }
            Ok(Attachment::ClipboardText(text)) => {
                if self.state.editor.pasting()
                    || !anchor.is_some_and(|at| self.state.editor.insert_clipboard_text(at, &text))
                {
                    self.state
                        .report(io, "Clipboard text could not fit in the draft");
                } else {
                    self.state.input_history.edited(&self.state.editor);
                    self.state.status.clear();
                }
            }
            Ok(Attachment::Image(image)) => {
                let reduced = image.original_byte_len() > image.byte_len();
                let editor = &mut self.state.editor;
                editor.attach_image(anchor.unwrap_or_else(|| editor.caret()), image);
                self.state.input_history.edited(editor);
                let count = editor.images.len();
                let notice = if reduced {
                    " · smaller copy for model"
                } else {
                    ""
                };
                if from_clipboard && !reduced {
                    self.state.status.clear();
                } else {
                    self.state
                        .report(io, &format!("Image attached ({count}){notice}"));
                }
            }
            Err("clipboard_unavailable") => self
                .state
                .report(io, "No clipboard image or text available"),
            Err("clipboard_size") => self
                .state
                .report(io, "Clipboard exceeds the attachment or draft size limit"),
            Err("clipboard_image_unavailable") => self.state.report(
                io,
                "Attach photos in an idle conversation with vision support",
            ),
            Err("protected_file") => self.state.report(io, "Protected files cannot be attached"),
            Err("attachment_outside_workspace" | "attachment_path") => self.state.report(
                io,
                "Use a direct path (no '..'); text files must be in the workspace",
            ),
            Err("attachment_open") => self.state.report(
                io,
                "Could not open attachment · use a readable file without symlinks",
            ),
            Err("attachment_regular_file_required") => self
                .state
                .report(io, "Attachment must be a regular file without hard links"),
            Err("attachment_text_encoding") => {
                self.state.report(io, "Use a non-empty UTF-8 text file");
            }
            Err("image_original_bytes") if self.state.editor.images.is_empty() => self
                .state
                .report(io, "Image exceeds 8 MiB. Attach a smaller copy."),
            Err("image_original_bytes" | "image_batch_bytes") => self.state.report(
                io,
                "Send the current attachments first, then add this image.",
            ),
            Err(code) => self
                .state
                .report(io, &format!("Could not attach file ({code})")),
        }
    }
}

impl Workbench {
    pub(super) fn paste_clipboard(&mut self, io: &Io) {
        let slot = &self.slots[self.current];
        if slot.state.attachment_loading || slot.state.attachment_submission.is_some() {
            self.report(io, "Wait for the attachment or submission");
            return;
        }
        if !self.can_job(io) {
            return;
        }
        let slot = &mut self.slots[self.current];
        let images_allowed = !self.factory.offline
            && matches!(slot.kind, Kind::Main | Kind::Fork | Kind::Btw)
            && self.orchestration_mode.is_none()
            && self.orchestration_stop.is_none()
            && self.refinement.is_none()
            && slot.state.available();
        let remaining = MAX_IMAGE_BYTES
            - slot
                .state
                .editor
                .attached_images()
                .map(ImageInput::byte_len)
                .sum::<usize>();
        let original_remaining = MAX_ORIGINAL_IMAGE_BYTES
            - slot
                .state
                .editor
                .attached_images()
                .map(ImageInput::original_byte_len)
                .sum::<usize>();
        let owner = slot.handle.incarnation().clone();
        slot.state.editor.clipboard_anchor = Some(slot.state.editor.caret());
        slot.state.attachment_loading = true;
        self.jobs.spawn_blocking(move || Job::Attachment {
            owner,
            from_clipboard: true,
            result: clipboard::load(images_allowed, remaining, original_remaining),
        });
    }

    pub(super) fn attach(&mut self, argument: &str, io: &Io) -> bool {
        let slot = &mut self.slots[self.current];
        if slot.state.attachment_loading || slot.state.attachment_submission.is_some() {
            self.report(io, "Wait for the attachment or submission");
            return false;
        }
        if argument == "--clear" {
            slot.state.clear_attachments();
            self.report(io, "Attachments cleared");
            return true;
        }
        let Some((text, path)) = parse_source(argument) else {
            self.report(io, USAGE);
            return false;
        };
        if !text && self.factory.offline {
            self.report(
                io,
                "Image attachments require a model route with vision support",
            );
            return false;
        }
        if !matches!(slot.kind, Kind::Main | Kind::Fork | Kind::Btw)
            || (text && !matches!(slot.state.mode, AgentMode::Basic | AgentMode::Plan))
            || self.orchestration_mode.is_some()
            || self.orchestration_stop.is_some()
            || self.refinement.is_some()
            || !slot.state.available()
        {
            self.report(io, "Attach files to an idle Work or Plan conversation");
            return false;
        }
        if text && slot.state.text_attachment.is_some() {
            self.report(
                io,
                "One text attachment per message · send it or /attach --clear",
            );
            return false;
        }
        if !self.can_job(io) {
            return false;
        }
        let path = path.to_owned();
        let cwd = self.factory.settings.cwd.clone();
        let state = ansa_linux::fs::normalize(&self.factory.settings.directory);
        let auth = self.factory.auth_path.clone();
        let slot = &mut self.slots[self.current];
        let owner = slot.handle.incarnation().clone();
        let remaining = MAX_IMAGE_BYTES
            - slot
                .state
                .editor
                .attached_images()
                .map(ImageInput::byte_len)
                .sum::<usize>();
        let original_remaining = MAX_ORIGINAL_IMAGE_BYTES
            - slot
                .state
                .editor
                .attached_images()
                .map(ImageInput::original_byte_len)
                .sum::<usize>();
        slot.state.attachment_loading = true;
        slot.state.report(io, "Reading attachment…");
        self.jobs.spawn_blocking(move || {
            let result = std::path::absolute(auth)
                .map_err(|_| "attachment_path")
                .and_then(|auth| {
                    load_attachment(
                        &cwd,
                        &state,
                        &ansa_linux::fs::normalize(&auth),
                        &path,
                        text,
                        remaining,
                        original_remaining,
                    )
                });
            Job::Attachment {
                owner,
                result,
                from_clipboard: false,
            }
        });
        true
    }
}

#[cfg(test)]
mod tests;
