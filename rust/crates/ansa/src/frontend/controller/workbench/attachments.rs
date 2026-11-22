//! Snapshot a user-selected file once, off the Carbon input loop.
use super::*;
use crate::provider::contracts::{MAX_IMAGE_BYTES, MAX_IMAGES};
use std::io::Read;

fn read_image(path: &Path, remaining: usize) -> Result<ImageInput> {
    let file = ansa_linux::fs::open_nofollow(path, ansa_linux::local::read_flags())
        .map_err(|_| "image_open")?;
    let metadata = file.metadata().map_err(|_| "image_stat")?;
    if !metadata.is_file() {
        return Err("image_regular_file_required");
    }
    if metadata.len() > remaining as u64 {
        return Err("image_too_large");
    }
    let mut bytes = Vec::new();
    file.take(remaining as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "image_read")?;
    if bytes.len() > remaining {
        return Err("image_too_large");
    }
    ImageInput::from_bytes(&bytes)
}

impl Workbench {
    pub(super) fn attach(&mut self, argument: &str, io: &Io) -> bool {
        let slot = &mut self.slots[self.current];
        if slot.state.image_loading || slot.state.image_submission.is_some() {
            self.report(io, "Wait for the image attachment or submission");
            return false;
        }
        if argument == "--clear" {
            slot.state.images.clear();
            self.report(io, "Image attachments cleared");
            return true;
        }
        if argument.is_empty() {
            self.report(io, "Use /attach PATH or /attach --clear");
            return false;
        }
        if self.factory.offline {
            self.report(
                io,
                "Image attachments require a model route with vision support",
            );
            return false;
        }
        if !matches!(slot.kind, Kind::Main | Kind::Fork | Kind::Btw)
            || self.orchestration_mode.is_some()
            || self.orchestration_stop.is_some()
            || self.refinement.is_some()
            || !slot.state.available()
        {
            self.report(io, "Attach images to an idle Work or Plan conversation");
            return false;
        }
        if slot.state.images.len() >= MAX_IMAGES {
            self.report(
                io,
                &format!("At most {MAX_IMAGES} images fit in one message"),
            );
            return false;
        }
        if !self.can_job(io) {
            return false;
        }
        let path = self.factory.settings.cwd.join(argument);
        let slot = &mut self.slots[self.current];
        let owner = slot.id.clone();
        let remaining = MAX_IMAGE_BYTES
            - slot
                .state
                .images
                .iter()
                .map(ImageInput::byte_len)
                .sum::<usize>();
        slot.state.image_loading = true;
        slot.state.report(io, "Reading image…");
        self.jobs.spawn_blocking(move || Job::Attachment {
            owner,
            result: read_image(&path, remaining),
        });
        true
    }
}
