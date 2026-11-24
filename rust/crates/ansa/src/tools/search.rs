//! Literal search with bounded pages over a stable workspace tree.
use super::files::{Host, append, b, error, result};
use crate::{
    auth::Cancel,
    config::{Result, n, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::Descriptor,
    tools::registry::Outcome,
};
use ansa_linux::fs::{DirectoryEntries, read_directory};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{File, Metadata},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileExt, MetadataExt},
    },
    path::{Path, PathBuf},
};

const BLOCK: usize = 64 * 1024;
// Keep the previous scan budget for new bytes per invocation (256 × 64 KiB).
// Pages stop on this budget even when no match was found.
const PAGE_BYTES: usize = 256 * BLOCK;
const PAGE_ENTRIES: usize = 4096;
// Leave room for the existing 64 KiB match budget in the tool result.
const CURSOR_BYTES: usize = 32 * 1024;
const LEGACY_DESCRIPTION: &str = "Bounded literal search: mode=name or content, max_results 1..50. Nested instruction scopes are returned separately.";

pub(super) fn descriptor(legacy: bool) -> Result<Descriptor> {
    let mut schema = super::files::schema(&[
        ("path", "string"),
        ("query", "string"),
        ("mode", "string"),
        ("max_results", "integer"),
    ])?;
    if !legacy {
        let props = schema.field("properties")?.with(
            "cursor",
            Json::parse(r#"{"type":"string","description":"Empty string for the first page, then use next_cursor verbatim"}"#)?,
        )?;
        let mut required = schema.field("required")?.elements()?.to_vec();
        required.push(s("cursor")?);
        schema = schema
            .with("properties", props)?
            .with("required", Json::array(required)?)?;
    }
    Ok(Descriptor {
        name: "search_files".into(),
        revision: if legacy { "ansa_local_v1" } else { "ansa_search_v2" }.into(),
        description: if legacy {
            LEGACY_DESCRIPTION
        } else {
            "Literal search by filename or file content. Returns up to max_results (1..50). Use next_cursor with the same path, query and mode until it is null, even after an empty page. Content matches include a byte offset for read_file. Nested AGENTS.md scopes are returned separately."
        }.into(),
        schema,
    })
}

pub(super) fn legacy_descriptor(current: &Descriptor) -> Option<Descriptor> {
    if current.name != "search_files" || current.revision != "ansa_search_v2" {
        return None;
    }
    descriptor(true).ok()
}

// Compare metadata of the open object so a cursor can explicitly report staleness.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp(u64, u64, u64, i64, i64, i64, i64);

impl From<Metadata> for Stamp {
    fn from(m: Metadata) -> Self {
        Self(
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    }
}

#[derive(Serialize, Deserialize)]
struct FilePosition {
    name: Vec<u8>,
    stamp: Stamp,
    offset: usize,
    line: usize,
    matched_line: bool,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    key: String,
    directories: String,
    tree: String,
    file: Option<FilePosition>,
}

struct Directory {
    path: PathBuf,
    entries: DirectoryEntries,
    name: Vec<u8>,
    cookie: i64,
    stamp: Stamp,
}

impl Directory {
    fn open(path: PathBuf, name: Vec<u8>) -> io::Result<Self> {
        let entries = read_directory(&path, false)?;
        Self::from_entries(path, name, entries)
    }

    fn from_entries(path: PathBuf, name: Vec<u8>, entries: DirectoryEntries) -> io::Result<Self> {
        let stamp = entries.metadata()?.into();
        Ok(Self {
            path,
            entries,
            name,
            cookie: 0,
            stamp,
        })
    }

    fn verify_stamp(&self) -> Result<()> {
        let stamp: Stamp = self
            .entries
            .metadata()
            .map_err(|_| "file_search_tree_changed")?
            .into();
        if stamp != self.stamp {
            return Err("file_search_tree_changed");
        }
        Ok(())
    }
}

fn component(name: &[u8]) -> Result<&OsStr> {
    if name.is_empty() || name.contains(&0) || name.contains(&b'/') || matches!(name, b"." | b"..")
    {
        return Err("file_search_cursor");
    }
    Ok(OsStr::from_bytes(name))
}

pub(super) fn cursor_argument(args: &Json) -> Result<Option<&str>> {
    let Ok(value) = args.field("cursor") else {
        return Ok(None);
    };
    let token = value.text()?;
    if token.len() > CURSOR_BYTES {
        return Err("file_search_cursor");
    }
    Ok((!token.is_empty()).then_some(token))
}

fn tree_stamp(stack: &[Directory]) -> Result<String> {
    let stamps: Vec<_> = stack.iter().map(|dir| dir.stamp).collect();
    Ok(sha256(
        &serde_json::to_vec(&stamps).map_err(|_| "file_search_cursor")?,
    ))
}

fn encode(key: String, stack: &[Directory], file: Option<FilePosition>) -> Result<Json> {
    // Names and directory cookies dominate a deep cursor. Encode each name
    // once using its byte length, then the native cookie in little endian.
    let mut positions = Vec::new();
    for dir in stack {
        positions.push(u8::try_from(dir.name.len()).map_err(|_| "file_search_cursor_limit")?);
        positions.extend_from_slice(&dir.name);
        positions.extend_from_slice(&dir.cookie.to_le_bytes());
    }
    let cursor = Cursor {
        key,
        directories: URL_SAFE_NO_PAD.encode(positions),
        tree: tree_stamp(stack)?,
        file,
    };
    let encoded = serde_json::to_string(&cursor).map_err(|_| "file_search_cursor")?;
    if encoded.len() > CURSOR_BYTES {
        return Err("file_search_cursor_limit");
    }
    s(&encoded)
}

struct Page {
    matches: Vec<Json>,
    scopes: Vec<Json>,
    encoded: usize,
    budget: usize,
    files: usize,
    maximum: usize,
}

impl Page {
    fn full(&self) -> bool {
        self.matches.len() >= self.maximum || self.scopes.len() >= 50 || self.budget == 0
    }
}

fn path_string(path: &Path) -> String {
    ansa_linux::fs::utf8_text(path.as_os_str().as_encoded_bytes())
}

impl Host {
    fn search_omits(&self, path: &Path, name: &OsStr) -> bool {
        self.protected_path(path)
            || self
                .settings
                .exclude
                .iter()
                .any(|value| value.as_bytes() == name.as_encoded_bytes())
    }

    fn search_directory(&self, path: &Path) -> bool {
        self.check_task_path(path, false).is_ok() || self.task_directory_allowed(path)
    }

    fn restore_search(
        &self,
        root: Directory,
        key: &str,
        token: Option<&str>,
        stop: &Cancel,
    ) -> Result<(Vec<Directory>, Option<FilePosition>)> {
        let Some(token) = token else {
            return Ok((vec![root], None));
        };
        let cursor: Cursor = serde_json::from_str(token).map_err(|_| "file_search_cursor")?;
        if cursor.key != key {
            return Err("file_search_cursor");
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(&cursor.directories)
            .map_err(|_| "file_search_cursor")?;
        let mut positions = bytes.as_slice();
        let mut path = root.path.clone();
        let mut root = Some(root);
        let mut directories: Vec<Directory> = Vec::new();
        while let Some((&length, rest)) = positions.split_first() {
            stop.check()?;
            let (name, rest) = rest
                .split_at_checked(length as usize)
                .ok_or("file_search_cursor")?;
            let (cookie, remainder) = rest.split_at_checked(8).ok_or("file_search_cursor")?;
            let cookie = i64::from_le_bytes(cookie.try_into().map_err(|_| "file_search_cursor")?);
            positions = remainder;
            if cookie < 0 {
                return Err("file_search_cursor");
            }
            let mut open = match directories.last() {
                None if name.is_empty() => root.take().ok_or("file_search_cursor")?,
                None => return Err("file_search_cursor"),
                Some(parent) => {
                    let name = component(name)?;
                    path.push(name);
                    if self.search_omits(&path, name) || !self.search_directory(&path) {
                        return Err("file_search_cursor_scope");
                    }
                    let entries = parent
                        .entries
                        .open_child(name)
                        .map_err(|_| "file_search_tree_changed")?;
                    let child = Directory::from_entries(
                        path.clone(),
                        name.as_encoded_bytes().to_vec(),
                        entries,
                    )
                    .map_err(|_| "file_search_tree_changed")?;
                    if child
                        .entries
                        .has_instructions()
                        .map_err(|_| "file_search_tree_changed")?
                    {
                        return Err("file_search_cursor_scope");
                    }
                    child
                }
            };
            open.entries
                .seek(cookie)
                .map_err(|_| "file_search_cursor")?;
            open.cookie = cookie;
            directories.push(open);
        }
        if directories.is_empty() || cursor.tree != tree_stamp(&directories)? {
            return Err("file_search_tree_changed");
        }
        if let Some(file) = &cursor.file {
            let name = component(&file.name)?;
            let path = path.join(name);
            if self.search_omits(&path, name) || self.check_task_path(&path, false).is_err() {
                return Err("file_search_cursor_scope");
            }
            if file.line == 0 || i64::try_from(file.offset).is_err() {
                return Err("file_search_cursor");
            }
        }
        Ok((directories, cursor.file))
    }

    pub fn search(&self, args: &Json, stop: &Cancel) -> Result<Outcome> {
        let root = self.task_directory_path(args.text_field("path")?)?;
        let opened = Directory::open(root.clone(), vec![]).map_err(|_| "file_access_failed")?;
        if let Some(value) = self.instructions(&root, args)? {
            return Ok(value);
        }
        opened.verify_stamp()?;
        let query = args.text_field("query")?.as_bytes();
        let mode = args.text_field("mode")?;
        let maximum = args.usize_field("max_results")?;
        if query.is_empty()
            || query.len() > 256
            || !matches!(mode, "name" | "content")
            || !(1..=50).contains(&maximum)
        {
            return Err("file_search_arguments");
        }
        let key = sha256(
            &serde_json::to_vec(&(root.as_os_str().as_encoded_bytes(), query, mode))
                .map_err(|_| "file_search_cursor")?,
        );
        let (mut stack, mut pending) =
            self.restore_search(opened, &key, cursor_argument(args)?, stop)?;
        if pending.is_some() && mode != "content" {
            return Err("file_search_cursor");
        }
        let mut page = Page {
            matches: vec![],
            scopes: vec![],
            encoded: 0,
            budget: PAGE_BYTES,
            files: 0,
            maximum,
        };
        let mut visited = 0;
        let mut buffer = vec![0; BLOCK + query.len() - 1];
        while !stack.is_empty() {
            if stop.cancelled() {
                return Ok(error("cancelled"));
            }
            if page.full() {
                break;
            }
            if let Some(position) = pending.as_mut() {
                for parent in &stack {
                    parent.verify_stamp()?;
                }
                let directory = stack.last().expect("nonempty directory stack");
                let name = component(&position.name)?;
                let path = directory.path.join(name);
                let file = directory
                    .entries
                    .open_regular(name)
                    .map_err(|_| "file_search_tree_changed")?;
                let stamp = file
                    .metadata()
                    .map_err(|_| "file_search_tree_changed")?
                    .into();
                if position.stamp != stamp {
                    return Err("file_search_tree_changed");
                }
                page.files += 1;
                let finished =
                    scan_file(&file, &path, position, query, &mut page, &mut buffer, stop)?;
                let after: Stamp = file
                    .metadata()
                    .map_err(|_| "file_search_tree_changed")?
                    .into();
                if position.stamp != after {
                    return Err("file_search_tree_changed");
                }
                for parent in &stack {
                    parent.verify_stamp()?;
                }
                if finished {
                    pending = None;
                } else {
                    break;
                }
                continue;
            }
            let directory = stack.last_mut().expect("nonempty directory stack");
            if visited == PAGE_ENTRIES || page.files == 256 {
                break;
            }
            let Some(entry) = directory.entries.next() else {
                directory.verify_stamp()?;
                stack.pop();
                continue;
            };
            visited += 1;
            let previous_cookie = directory.cookie;
            directory.cookie = directory.entries.offset();
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => continue,
                Err(_) => return Err("file_access_failed"),
            };
            let path = directory.path.join(&entry.name);
            let name = entry.name.as_encoded_bytes();
            if entry.symlink || self.search_omits(&path, &entry.name) {
                continue;
            }
            if entry.directory {
                if !self.search_directory(&path) {
                    continue;
                }
                let entries = match directory.entries.open_child(&entry.name) {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => continue,
                    Err(_) => return Err("file_search_tree_changed"),
                };
                let child = Directory::from_entries(path, name.to_vec(), entries)
                    .map_err(|_| "file_access_failed")?;
                if child
                    .entries
                    .has_instructions()
                    .map_err(|_| "file_access_failed")?
                {
                    if !append(
                        &mut page.scopes,
                        s(crate::config::path_text(&child.path)?)?,
                        &mut page.encoded,
                    ) {
                        directory.cookie = previous_cookie;
                        break;
                    }
                } else {
                    stack.push(child);
                }
                continue;
            }
            if !entry.regular || self.check_task_path(&path, false).is_err() {
                continue;
            }
            if mode == "name" {
                if name.windows(query.len()).any(|window| window == query)
                    && !append(
                        &mut page.matches,
                        Json::object(vec![("path", s(&path_string(&path))?)])?,
                        &mut page.encoded,
                    )
                {
                    directory.cookie = previous_cookie;
                    break;
                }
            } else if let Ok(file) = directory.entries.open_regular(&entry.name) {
                let stamp = file
                    .metadata()
                    .map_err(|_| "file_search_tree_changed")?
                    .into();
                pending = Some(FilePosition {
                    name: name.to_vec(),
                    stamp,
                    offset: 0,
                    line: 1,
                    matched_line: false,
                });
            }
        }
        if stop.cancelled() {
            return Ok(error("cancelled"));
        }
        for directory in &stack {
            directory.verify_stamp()?;
        }
        let truncated = !stack.is_empty();
        let cursor = if truncated {
            encode(key, &stack, pending)?
        } else {
            Json::parse("null")?
        };
        Ok(result(Json::object(vec![
            ("matches", Json::array(page.matches)?),
            ("instruction_scopes", Json::array(page.scopes)?),
            ("truncated", b(truncated)),
            ("files_scanned", n(page.files)),
            ("next_cursor", cursor),
        ])?))
    }
}

fn scan_file(
    file: &File,
    path: &Path,
    pos: &mut FilePosition,
    query: &[u8],
    page: &mut Page,
    buffer: &mut [u8],
    stop: &Cancel,
) -> Result<bool> {
    while !page.full() && !stop.cancelled() {
        // Retain only the overlap needed to recognize a literal split across
        // blocks. After a newline, each line has its own matching state.
        let overlap = pos.offset.min(query.len() - 1);
        let start = pos.offset - overlap;
        let available = i64::MAX as u64 - start as u64;
        let count =
            ((BLOCK.min(page.budget) + overlap).min(buffer.len()) as u64).min(available) as usize;
        if count <= overlap {
            return Ok(true);
        }
        let mut read = 0;
        while read < count {
            match file.read_at(&mut buffer[read..count], (start + read) as u64) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err("file_search_read_failed"),
            }
        }
        if read <= overlap || buffer[..read].contains(&0) {
            return Ok(true);
        }
        page.budget -= read - overlap;
        let available = &buffer[..read];
        let mut from = available[..overlap]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |n| n + 1);
        for segment in available[from..].split_inclusive(|&b| b == b'\n') {
            let new_line = segment.last() == Some(&b'\n');
            let line = if new_line {
                &segment[..segment.len() - 1]
            } else {
                segment
            };
            if !pos.matched_line
                && let Some(found) = line
                    .windows(query.len())
                    .enumerate()
                    .find_map(|(i, bytes)| {
                        (from + i + query.len() > overlap && bytes == query).then_some(i)
                    })
            {
                let preview_start = if found + query.len() > 256 { found } else { 0 };
                let preview_end = (preview_start + 256).min(line.len());
                let entry = Json::object(vec![
                    ("path", s(&path_string(path))?),
                    ("line", n(pos.line)),
                    ("offset", n(start + from + found)),
                    (
                        "text",
                        s(&ansa_linux::fs::utf8_text(
                            &line[preview_start..preview_end],
                        ))?,
                    ),
                ])?;
                if !append(&mut page.matches, entry, &mut page.encoded) {
                    return Ok(false);
                }
                pos.matched_line = true;
            }
            from += segment.len();
            pos.offset = start + from;
            if new_line {
                pos.line = pos.line.checked_add(1).ok_or("file_search_cursor")?;
                pos.matched_line = false;
            }
            // Finish any lines in a budget-consuming block before issuing a
            // continuation. The next block will start at the saved offset.
            if page.matches.len() >= page.maximum {
                return Ok(false);
            }
        }
        if read < count {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{agent_epoch, load_agent_settings},
        tools::{
            files::local_tools,
            registry::{Definition, Registry},
        },
    };
    use std::{fs, io::Write, sync::Arc};

    struct Fixture {
        root: PathBuf,
        path: PathBuf,
        token: String,
        tools: Arc<crate::tools::registry::Snapshot>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "ansa-search-tests-{}",
                crate::auth::pkce::random_token(16).unwrap()
            ));
            ansa_linux::fs::create_private_leaf(&root).unwrap();
            let path = root.join("workspace");
            fs::create_dir(&path).unwrap();
            let settings = load_agent_settings(&path, &root).unwrap();
            let epoch = agent_epoch(&settings).unwrap();
            let token = sha256(
                epoch
                    .exact_state
                    .field("instructions")
                    .unwrap()
                    .dump()
                    .as_bytes(),
            );
            let tools = local_tools(
                settings,
                epoch,
                &root.join("session"),
                &root.join("auth"),
                &std::env::current_exe().unwrap(),
            )
            .unwrap();
            Self {
                root,
                path,
                token,
                tools,
            }
        }

        fn search(&self, mode: &str, query: &str, cursor: Json) -> Json {
            let tool = self
                .tools
                .definitions
                .iter()
                .find(|tool| tool.descriptor.name == "search_files")
                .unwrap();
            let args = Json::object(vec![
                ("path", s(".").unwrap()),
                ("mode", s(mode).unwrap()),
                ("query", s(query).unwrap()),
                ("max_results", n(1)),
                ("cursor", cursor),
                ("instruction_token", s(&self.token).unwrap()),
            ])
            .unwrap();
            assert!(crate::tools::schema::check_arguments(&tool.descriptor.schema, &args).is_ok());
            assert!((tool.validator)(&args));
            let value = (tool.executor)(&args);
            Json::parse(std::str::from_utf8(value.output()).unwrap()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn finds_literals_split_across_blocks_and_pages_without_duplicate_lines() {
        let fixture = Fixture::new();
        let mut log = File::create(fixture.path.join("build.log")).unwrap();
        let padding = vec![b'x'; BLOCK - 3];
        log.write_all(&padding).unwrap();
        log.write_all(b"needle needle\n").unwrap();
        let mut bytes = BLOCK - 3 + b"needle needle\n".len();
        let mut remainder = PAGE_BYTES - 3 - bytes;
        while remainder > 0 {
            let count = remainder.min(padding.len());
            log.write_all(&padding[..count]).unwrap();
            remainder -= count;
            bytes += count;
        }
        assert_eq!(bytes, PAGE_BYTES - 3);
        log.write_all(b"needle needle\nneedle at EOF").unwrap();

        let mut cursor = s("").unwrap();
        let mut lines = Vec::new();
        for _ in 0..7 {
            let page = fixture.search("content", "needle", cursor);
            cursor = page.field("next_cursor").unwrap().clone();
            for entry in page.field("matches").unwrap().elements().unwrap() {
                lines.push((
                    entry.usize_field("line").unwrap(),
                    entry.usize_field("offset").unwrap(),
                ));
                assert!(entry.text_field("text").unwrap().contains("needle"));
            }
            if cursor.is_null() {
                break;
            }
        }
        assert_eq!(
            lines,
            [
                (1, BLOCK - 3),
                (2, PAGE_BYTES - 3),
                (3, PAGE_BYTES - 3 + b"needle needle\n".len())
            ]
        );
        assert!(cursor.is_null());
    }

    #[test]
    fn empty_page_continues_into_a_literal_split_at_the_scan_budget() {
        let fixture = Fixture::new();
        let mut log = File::create(fixture.path.join("tail.log")).unwrap();
        let block = vec![b'x'; BLOCK];
        for _ in 1..PAGE_BYTES / BLOCK {
            log.write_all(&block).unwrap();
        }
        log.write_all(&block[..BLOCK - 2]).unwrap();
        log.write_all(b"needle at the end\n").unwrap();

        let first = fixture.search("content", "needle", s("").unwrap());
        assert!(
            first
                .field("matches")
                .unwrap()
                .elements()
                .unwrap()
                .is_empty()
        );
        let cursor = first.field("next_cursor").unwrap().clone();
        let next = fixture.search("content", "needle", cursor);
        let matches = next.field("matches").unwrap().elements().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].usize_field("offset"), Ok(PAGE_BYTES - 2));
        assert_eq!(matches[0].usize_field("line"), Ok(1));
    }

    #[test]
    fn resumed_content_cannot_enter_new_instruction_scope() {
        let fixture = Fixture::new();
        let group = fixture.path.join("group");
        fs::create_dir(&group).unwrap();
        fs::write(group.join("file"), "needle\nneedle\n").unwrap();
        let first = fixture.search("content", "needle", s("").unwrap());
        let cursor = first.field("next_cursor").unwrap().clone();
        assert!(!cursor.is_null());
        fs::write(group.join("AGENTS.md"), "New scope").unwrap();
        let following = fixture.search("content", "needle", cursor);
        assert_eq!(
            following.text_field("error"),
            Ok("file_search_cursor_scope")
        );
    }

    #[test]
    fn name_pages_continue_the_tree_and_restart_after_a_directory_changes() {
        let fixture = Fixture::new();
        let group = fixture.path.join("group");
        let private = fixture.path.join("instruction-scope");
        fs::create_dir(&group).unwrap();
        fs::create_dir(&private).unwrap();
        fs::write(private.join("AGENTS.md"), "Instructions").unwrap();
        fs::write(private.join("marker-hidden"), "").unwrap();
        for path in [
            fixture.path.join("marker-first"),
            fixture.path.join("marker-last"),
            group.join("marker-child"),
        ] {
            fs::write(path, "").unwrap();
        }

        let mut cursor = s("").unwrap();
        let mut paths = Vec::new();
        let mut scopes = Vec::new();
        for _ in 0..6 {
            let page = fixture.search("name", "marker", cursor);
            cursor = page.field("next_cursor").unwrap().clone();
            for entry in page.field("matches").unwrap().elements().unwrap() {
                paths.push(entry.text_field("path").unwrap().to_owned());
            }
            for entry in page
                .field("instruction_scopes")
                .unwrap()
                .elements()
                .unwrap()
            {
                scopes.push(entry.text().unwrap().to_owned());
            }
            if cursor.is_null() {
                break;
            }
        }
        assert!(cursor.is_null());
        paths.sort();
        assert_eq!(
            paths,
            ["group/marker-child", "marker-first", "marker-last"]
                .map(|name| path_string(&fixture.path.join(name)))
        );
        assert_eq!(scopes, [path_string(&private)]);

        let page = fixture.search("name", "marker", s("").unwrap());
        let cursor = page.field("next_cursor").unwrap().clone();
        assert_eq!(
            fixture
                .search("name", "different", cursor.clone())
                .text_field("error"),
            Ok("file_search_cursor")
        );
        fs::write(fixture.path.join("created-after-page"), "").unwrap();
        assert_eq!(
            fixture.search("name", "marker", cursor).text_field("error"),
            Ok("file_search_tree_changed")
        );
    }

    #[test]
    fn saved_search_catalog_keeps_its_original_schema_and_revision() {
        let fixture = Fixture::new();
        let tool = fixture
            .tools
            .definitions
            .iter()
            .find(|tool| tool.descriptor.name == "search_files")
            .unwrap();
        let mut registry = Registry::new();
        let saved = descriptor(true).unwrap();
        assert_eq!(saved.revision, "ansa_local_v1");
        assert!(!saved.schema.field("properties").unwrap().has("cursor"));
        registry
            .register_with_policy(
                Definition {
                    descriptor: saved.clone(),
                    ..tool.clone()
                },
                fixture.tools.policy("search_files").unwrap().clone(),
            )
            .unwrap();
        let old = registry.snapshot();
        let accepted = fixture.tools.compatible(old.advertisement()).unwrap();
        assert_eq!(accepted.advertisement(), old.advertisement());
    }
}
