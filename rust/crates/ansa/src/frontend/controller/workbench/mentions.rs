//! Resolve image references in the submitted draft without blocking the input loop.
use super::*;
use crate::provider::contracts::{MAX_IMAGE_BYTES, MAX_IMAGES, image::MAX_ORIGINAL_IMAGE_BYTES};

impl Workbench {
    pub(super) fn prepare_mention_images(&mut self, text: &str, io: &Io) -> bool {
        if self.orchestration_mode.is_some() || self.refinement.is_some() {
            return false;
        }
        let slot = &self.slots[self.current];
        let argument = if text.trim_start().starts_with('/') {
            let Some(argument) = commands::skill_argument(&slot.skill_commands, text) else {
                return false;
            };
            argument
        } else {
            text
        };
        let paths = commands::mention::image_paths(argument, MAX_IMAGES);
        if paths.is_empty() {
            return false;
        }
        if self.factory.offline {
            return false;
        }
        if !slot.state.available()
            || slot.state.attachment_loading
            || slot.state.attachment_submission.is_some()
            || self.collecting_orchestration()
        {
            self.report(io, "Wait for the current turn before sending images");
            return true;
        }
        if paths.len() + slot.state.images.len() > MAX_IMAGES {
            self.report(
                io,
                &format!("At most {MAX_IMAGES} images fit in one message"),
            );
            return true;
        }
        if !self.can_job(io) {
            return true;
        }
        let slot = &mut self.slots[self.current];
        let owner = slot.id.clone();
        let mode = slot.state.mode;
        let text = text.to_owned();
        let mut remaining = MAX_IMAGE_BYTES
            - slot
                .state
                .images
                .iter()
                .map(ImageInput::byte_len)
                .sum::<usize>();
        let mut originals = MAX_ORIGINAL_IMAGE_BYTES
            - slot
                .state
                .images
                .iter()
                .map(ImageInput::original_byte_len)
                .sum::<usize>();
        let host = self.files.clone();
        let stop = Cancel::linked(&self.stop);
        slot.state.attachment_loading = true;
        slot.sending_mention = true;
        slot.state.report(io, "Reading mentioned images…");
        self.jobs.spawn_blocking(move || {
            let result = paths
                .iter()
                .map(|path| {
                    stop.check()?;
                    let (_, file) = host.open_regular(Path::new(path))?;
                    let image = attachments::read_open_image(file, remaining, originals)?;
                    remaining -= image.byte_len();
                    originals -= image.original_byte_len();
                    Ok(image)
                })
                .collect();
            Job::MentionImages {
                owner,
                text,
                mode,
                result,
            }
        });
        true
    }

    pub(super) fn submit_mention_images(
        &mut self,
        owner: &str,
        text: &str,
        mode: AgentMode,
        result: Result<Vec<ImageInput>>,
        io: &Io,
    ) {
        let Some(index) = self.slots.iter().position(|s| s.id == owner) else {
            return;
        };
        let slot = &mut self.slots[index];
        slot.state.attachment_loading = false;
        if !std::mem::take(&mut slot.sending_mention)
            || index != self.current
            || slot.state.editor.text() != Ok(text)
            || slot.state.mode != mode
            || self.terminating
        {
            return;
        }
        match result {
            Ok(images) => {
                let explicit = slot.state.images.len();
                slot.state.images.extend(images);
                self.submit_composer(text, io);
                self.slots[index].state.images.truncate(explicit);
            }
            Err(code) => slot
                .state
                .report(io, &format!("Could not attach mentioned image ({code})")),
        }
    }

    pub(super) fn submit_composer(&mut self, text: &str, io: &Io) -> bool {
        let origin = self.current;
        let (accepted, done) = self.text(text, io);
        if accepted {
            self.selection = None;
            let slot = &mut self.slots[origin];
            if let Some((id, _, _)) = &slot.state.pending_draft
                && slot.state.pending.as_ref() == Some(id)
            {
                slot.follow_accepted_submit = Some(id.clone());
            }
            if !matches!(self.menu, Some(Menu::DraftingPlan)) {
                slot.state.editor.clear();
                slot.state.input_history.reset();
            }
            slot.refresh = Instant::now();
        }
        done
    }
}
