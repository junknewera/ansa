//! Clipboard is read only in response to an explicit paste in the composer.
use std::{
    io::{ErrorKind, Read},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use super::{Attachment, ImageInput, Result};

fn capture(mut command: Command, limit: usize, deadline: Instant) -> Result<Option<Vec<u8>>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "clipboard_helper")?;
    let result = (|| {
        let mut pipe = child.stdout.take().ok_or("clipboard_helper")?;
        ansa_linux::managed::nonblocking(&pipe).map_err(|_| "clipboard_helper")?;
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        let mut eof = false;
        while Instant::now() < deadline {
            if !eof {
                match pipe.read(&mut chunk) {
                    Ok(0) => eof = true,
                    Ok(n) if bytes.len() + n <= limit => {
                        bytes.extend_from_slice(&chunk[..n]);
                        continue;
                    }
                    Ok(_) => return Err("clipboard_size"),
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => return Err("clipboard_read"),
                }
            }
            if eof && let Some(status) = child.try_wait().map_err(|_| "clipboard_helper")? {
                return Ok((status.success() && !bytes.is_empty()).then_some(bytes));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err("clipboard_timeout")
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

pub(super) fn load(
    images_allowed: bool,
    image_limit: usize,
    original_limit: usize,
) -> Result<Attachment> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let backends = [
        (
            true,
            "WAYLAND_DISPLAY",
            option_env!("ANSA_WL_PASTE_BIN").unwrap_or("wl-paste"),
        ),
        (
            false,
            "DISPLAY",
            option_env!("ANSA_XCLIP_BIN").unwrap_or("xclip"),
        ),
    ];
    for (wayland, display, program) in backends {
        if std::env::var_os(display).is_none_or(|v| v.is_empty()) {
            continue;
        }
        for (image, mime) in [(true, "image/png"), (true, "image/jpeg"), (false, "text")] {
            let mut command = Command::new(program);
            if wayland {
                command.args(["--no-newline", "--type", mime]);
            } else {
                let target = if image { mime } else { "UTF8_STRING" };
                command.args(["-selection", "clipboard", "-out", "-target", target]);
            }
            let limit = if image {
                original_limit
            } else {
                crate::frontend::input::MAX_COMPOSER_BYTES
            };
            match capture(command, limit, deadline) {
                Ok(Some(bytes)) if image => {
                    if !images_allowed {
                        return Err("clipboard_image_unavailable");
                    }
                    return ImageInput::fit(&bytes, image_limit).map(Attachment::Image);
                }
                Ok(Some(bytes)) => {
                    return String::from_utf8(bytes)
                        .map(Attachment::ClipboardText)
                        .map_err(|_| "clipboard_encoding");
                }
                Ok(None) => {}
                Err("clipboard_helper") => break,
                Err(code) => return Err(code),
            }
        }
    }
    Err("clipboard_unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_helper_output_is_bounded() {
        let make_command = || {
            let mut command = Command::new("printf");
            command.arg("abc");
            command
        };
        let deadline = || Instant::now() + Duration::from_secs(2);
        assert_eq!(
            capture(make_command(), 3, deadline()),
            Ok(Some(b"abc".to_vec()))
        );
        assert_eq!(
            capture(make_command(), 2, deadline()),
            Err("clipboard_size")
        );
    }
}
