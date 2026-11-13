use super::files::{Host, append, b, error, regular, result};
use crate::{
    auth::Cancel,
    config::{Result, n, s},
    protocol::json::Json,
    tools::registry::Outcome,
};
use std::io::Read;
impl Host {
    pub fn search(&self, args: &Json, stop: &Cancel) -> Result<Outcome> {
        let root = self.path(args.text_field("path")?)?;
        if let Some(value) = self.instructions(&root, args)? {
            return Ok(value);
        }
        let query = args.text_field("query")?;
        let mode = args.text_field("mode")?;
        let maximum = args.usize_field("max_results")?;
        if query.is_empty()
            || query.len() > 256
            || !matches!(mode, "name" | "content")
            || !(1..=50).contains(&maximum)
        {
            return Err("file_search_arguments");
        }
        let mut stack = vec![std::fs::read_dir(&root).map_err(|_| "file_access_failed")?];
        let (mut matches, mut scopes) = (vec![], vec![]);
        let (mut visited, mut scanned, mut bytes, mut truncated) = (0, 0, 0, false);
        while let Some(entries) = stack.last_mut() {
            let entry = match entries.next() {
                Some(Ok(entry)) => entry,
                Some(Err(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                Some(Err(_)) => return Err("file_access_failed"),
                None => {
                    stack.pop();
                    continue;
                }
            };
            if stop.cancelled() {
                return Ok(error("cancelled"));
            }
            visited += 1;
            if visited > 4096 || scanned >= 256 || matches.len() >= maximum {
                truncated = true;
                break;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.as_encoded_bytes();
            if ansa_linux::fs::protected_path(&path)
                || path.starts_with(&self.auth)
                || path.starts_with(self.settings.directory.join("auth"))
                || path.starts_with(self.settings.directory.join("sessions"))
                || self.settings.exclude.iter().any(|v| v.as_bytes() == name)
            {
                continue;
            }
            let kind = entry.file_type().map_err(|_| "file_access_failed")?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if path.join("AGENTS.md").exists() {
                    if scopes.len() < 50
                        && !append(
                            &mut scopes,
                            s(crate::config::path_text(&path)?)?,
                            &mut bytes,
                        )
                    {
                        truncated = true;
                    }
                } else {
                    match std::fs::read_dir(&path) {
                        Ok(entries) => stack.push(entries),
                        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
                        Err(_) => return Err("file_access_failed"),
                    }
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let file = self.path_os(&path)?;
            if mode == "name" {
                if name.windows(query.len()).any(|v| v == query.as_bytes())
                    && !append(
                        &mut matches,
                        Json::object(vec![("path", s(&path_string(&file))?)])?,
                        &mut bytes,
                    )
                {
                    truncated = true;
                    break;
                }
                continue;
            }
            scanned += 1;
            let mut fd = match regular(&file) {
                Ok(fd) => fd,
                Err(_) => continue,
            };
            let mut content = vec![0; 65537];
            let count = match fd.read(&mut content) {
                Ok(0) | Err(_) => continue,
                Ok(count) => count,
            };
            truncated |= count > 65536;
            content.truncate(count.min(65536));
            if content.contains(&0) {
                continue;
            }
            let mut start = 0;
            let mut line = 1;
            while start < content.len() && matches.len() < maximum {
                let end = content[start..]
                    .iter()
                    .position(|v| *v == b'\n')
                    .map(|v| v + start)
                    .unwrap_or(content.len());
                let text = &content[start..end];
                if text.windows(query.len()).any(|v| v == query.as_bytes())
                    && !append(
                        &mut matches,
                        Json::object(vec![
                            ("path", s(&path_string(&file))?),
                            ("line", n(line)),
                            (
                                "text",
                                s(&ansa_linux::fs::utf8_text(&text[..text.len().min(256)]))?,
                            ),
                        ])?,
                        &mut bytes,
                    )
                {
                    truncated = true;
                    break;
                }
                if end == content.len() {
                    break;
                }
                start = end + 1;
                line += 1;
            }
        }
        Ok(result(Json::object(vec![
            ("matches", Json::array(matches)?),
            ("instruction_scopes", Json::array(scopes)?),
            ("truncated", b(truncated)),
            ("files_scanned", n(scanned)),
        ])?))
    }
}
fn path_string(path: &std::path::Path) -> String {
    ansa_linux::fs::utf8_text(path.as_os_str().as_encoded_bytes())
}
