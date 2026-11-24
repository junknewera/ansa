//! Bounded, operator-installed directory skills. Only descriptions go in
//! provider instructions before selection.
use super::{Result, digest_ok};
use crate::protocol::legacy_json_v1::sha256;
use ansa_linux::{fs, local};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::{FileExt, MetadataExt},
    path::Path,
    sync::Mutex,
};

const INLINE_BODY: usize = 16 * 1024;
pub(super) const PAGE_BYTES: usize = 8 * 1024;
const MAX_HEADER: usize = 4 * 1024;
const MAX_DESCRIPTION: usize = 512;
const MAX_LEGACY_SNAPSHOT: usize = 64 * 1024;
// Keep the catalog metadata within the tool output budget. Body bytes are
// stored and streamed independently.
const MAX_INDEX: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct DirectorySkills {
    entries: Vec<Skill>,
    directory: Option<File>,
    legacy_bodies: BTreeMap<String, String>,
    verified: Mutex<BTreeMap<String, BodyStamp>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Skill {
    name: String,
    description: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySkill {
    name: String,
    description: String,
    body: String,
}

/// Immutable input referenced by the existing harness epoch pin.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot<T> {
    version: u8,
    scope: String,
    profile: Option<String>,
    skills: T,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SavedCatalog {
    Index(Snapshot<Vec<Skill>>),
    Legacy(Snapshot<Vec<LegacySkill>>),
}

fn publish(dir: &File, name: &str, data: &[u8]) -> Result<()> {
    let existing = || {
        let bytes = local::read(dir, name, data.len()).map_err(|_| "skill_snapshot_read")?;
        match bytes {
            Some(bytes) if bytes == data => Ok(true),
            Some(_) => Err("skill_snapshot_digest"),
            None => Ok(false),
        }
    };
    if existing()? {
        return Ok(());
    }
    let suffix = suffix()?;
    match local::create_immutable(dir, name, data, &suffix) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && existing()? => Ok(()),
        Err(_) => Err("skill_snapshot_publish"),
    }
}

fn suffix() -> Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut random = [0; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| "skill_snapshot_publish")?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl DirectorySkills {
    pub(super) fn load(
        dir: File,
        scope: &str,
        revision: &str,
    ) -> Result<Option<(Option<String>, Self)>> {
        if !digest_ok(revision) {
            return Err("harness_revision");
        }
        let Some(bytes) = local::read(&dir, &format!("catalog-{revision}"), MAX_INDEX)
            .map_err(|_| "skill_snapshot_read")?
        else {
            return Ok(None);
        };
        if sha256(&bytes) != revision {
            return Err("skill_snapshot_digest");
        }
        let saved: SavedCatalog =
            serde_json::from_slice(&bytes).map_err(|_| "skill_snapshot_invalid")?;
        let mut skills = Self::default();
        let (saved_scope, profile, entries) = match saved {
            SavedCatalog::Index(snapshot) if snapshot.version == 2 => {
                skills.directory = Some(dir);
                (snapshot.scope, snapshot.profile, snapshot.skills)
            }
            SavedCatalog::Legacy(snapshot)
                if snapshot.version == 1 && bytes.len() <= MAX_LEGACY_SNAPSHOT =>
            {
                let mut entries = Vec::new();
                for skill in snapshot.skills {
                    if !valid_body(&skill.body) {
                        return Err("skill_snapshot_invalid");
                    }
                    let digest = sha256(skill.body.as_bytes());
                    skills.legacy_bodies.insert(digest.clone(), skill.body);
                    entries.push(Skill {
                        name: skill.name,
                        description: skill.description,
                        sha256: digest,
                    });
                }
                (snapshot.scope, snapshot.profile, entries)
            }
            _ => return Err("skill_snapshot_invalid"),
        };
        if saved_scope != scope || profile.as_deref().is_some_and(|pin| !digest_ok(pin)) {
            return Err("skill_snapshot_invalid");
        }
        skills.entries = entries;
        skills.validate()?;
        Ok(Some((profile, skills)))
    }

    pub(super) fn pin(
        mut self,
        dir: File,
        scope: &str,
        profile: Option<&str>,
    ) -> Result<(String, Self)> {
        self.validate()?;
        let index = Snapshot {
            version: 2,
            scope: scope.to_owned(),
            profile: profile.map(str::to_owned),
            skills: &self.entries,
        };
        let bytes = serde_json::to_vec(&index).map_err(|_| "skill_snapshot_invalid")?;
        let revision = sha256(&bytes);
        publish(&dir, &format!("catalog-{revision}"), &bytes)?;
        dir.sync_all().map_err(|_| "skill_snapshot_publish")?;
        self.directory = Some(dir);
        Ok((revision, self))
    }

    pub(super) fn pin_body(
        dir: &File,
        digest: &str,
        source: &mut BufReader<File>,
        start: u64,
    ) -> Result<()> {
        let name = format!("body-{digest}");
        let existing = || {
            let Some(file) =
                local::open_private_read(dir, &name).map_err(|_| "skill_snapshot_read")?
            else {
                return Ok(false);
            };
            let (actual, _) = scan_file(&mut BufReader::new(file), 0, |_| Ok(()))
                .map_err(|_| "skill_snapshot_read")?;
            if actual != digest {
                return Err("skill_snapshot_digest");
            }
            Ok(true)
        };
        if existing()? {
            return Ok(());
        }
        match local::create_immutable_with(dir, &name, &suffix()?, |file| {
            let (actual, _) = scan_file(source, start, |chunk| file.write_all(chunk.as_bytes()))?;
            if actual != digest {
                return Err(io::Error::other("skill_source_changed"));
            }
            Ok(())
        }) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && existing()? => Ok(()),
            Err(_) => Err("skill_snapshot_publish"),
        }
    }
}

fn valid_body(body: &str) -> bool {
    !body.trim().is_empty() && body.len() <= INLINE_BODY && valid_text(body)
}

fn valid_text(body: &str) -> bool {
    body.chars()
        .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct BodyStamp {
    device: u64,
    inode: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl BodyStamp {
    fn read(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self {
            device: meta.dev(),
            inode: meta.ino(),
            len: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        })
    }
}

fn scan_file(
    source: &mut BufReader<File>,
    start: u64,
    visit: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<(String, BodyStamp)> {
    let stamp = BodyStamp::read(source.get_ref())?;
    let len = stamp
        .len
        .checked_sub(start)
        .ok_or_else(|| io::Error::other("skill_body_size"))?;
    source.seek(SeekFrom::Start(start))?;
    let mut bounded = source.take(len);
    let digest = scan_body(&mut bounded, visit)?;
    if bounded.limit() != 0 || BodyStamp::read(bounded.into_inner().get_ref())? != stamp {
        return Err(io::Error::other("skill_source_changed"));
    }
    Ok((digest, stamp))
}

fn scan_body(
    mut source: impl Read,
    mut visit: impl FnMut(&str) -> io::Result<()>,
) -> io::Result<String> {
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0; PAGE_BYTES + 3];
    let mut remainder = 0;
    let mut nonempty = false;
    loop {
        let count = source.read(&mut buffer[remainder..])?;
        if count == 0 {
            if remainder != 0 || !nonempty {
                return Err(io::Error::other("skill_body_invalid"));
            }
            break;
        }
        let end = remainder + count;
        let complete = match std::str::from_utf8(&buffer[..end]) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&buffer[..error.valid_up_to()])
                    .map_err(|_| io::Error::other("skill_body_invalid"))?
            }
            Err(_) => return Err(io::Error::other("skill_body_invalid")),
        };
        if !valid_text(complete) {
            return Err(io::Error::other("skill_body_invalid"));
        }
        nonempty |= !complete.trim().is_empty();
        hash.update(complete.as_bytes());
        visit(complete)?;
        let size = complete.len();
        buffer.copy_within(size..end, 0);
        remainder = end - size;
    }
    let digest = hash
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(digest)
}

impl Skill {
    fn valid(&self) -> bool {
        self.valid_metadata() && digest_ok(&self.sha256)
    }

    fn valid_metadata(&self) -> bool {
        valid_name(&self.name)
            && !self.description.trim().is_empty()
            && self.description.len() <= MAX_DESCRIPTION
            && !self.description.chars().any(char::is_control)
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn scalar(value: &str) -> Option<String> {
    let value = value.trim();
    match value.as_bytes().first()? {
        b'"' => serde_json::from_str::<String>(value).ok(),
        b'\'' => {
            let mut rest = value.strip_prefix('\'')?.strip_suffix('\'')?;
            let mut decoded = String::new();
            while let Some((prefix, suffix)) = rest.split_once('\'') {
                decoded.push_str(prefix);
                decoded.push('\'');
                rest = suffix.strip_prefix('\'')?;
            }
            decoded.push_str(rest);
            Some(decoded)
        }
        b'&' | b'*' | b'!' | b'{' | b'[' | b'|' | b'>' | b'#' => None,
        _ => {
            let plain = value.split_once(" #").map_or(value, |(head, _)| head);
            (!plain.contains(": ") && !plain.ends_with(':')).then(|| plain.trim_end().to_owned())
        }
    }
}

fn read_header(source: &mut BufReader<File>) -> Result<String> {
    let mut bounded = source.take((MAX_HEADER + 10) as u64);
    let mut line = String::new();
    bounded
        .read_line(&mut line)
        .map_err(|_| "skill_header_invalid")?;
    if !matches!(line.as_str(), "---\n" | "---\r\n") {
        return Err("skill_header_invalid");
    }
    let mut header = String::new();
    loop {
        line.clear();
        bounded
            .read_line(&mut line)
            .map_err(|_| "skill_header_invalid")?;
        if matches!(line.as_str(), "---\n" | "---\r\n") {
            return Ok(header);
        }
        if line.is_empty() || header.len() + line.len() > MAX_HEADER {
            return Err("skill_header_invalid");
        }
        header.push_str(&line);
    }
}

fn parse_header(folder: &str, header: &str) -> Option<Skill> {
    let mut fields = header.lines().peekable();
    let mut name = None;
    let mut description = None;
    while let Some(line) = fields.next() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let (field, value) = line.split_once(':')?;
        let value = value.trim();
        match field {
            "name" if name.is_none() => name = Some(scalar(value)?),
            "description" if description.is_none() => {
                let text = if matches!(value, ">" | ">-" | "|" | "|-") {
                    // Descriptions are display text, so both block forms are
                    // normalized to one line; the Markdown body is untouched.
                    let mut block = String::new();
                    while let Some(line) = fields.next_if(|line| line.starts_with(' ')) {
                        let line = line.trim();
                        if !line.is_empty() {
                            if !block.is_empty() {
                                block.push(' ');
                            }
                            block.push_str(line);
                        }
                    }
                    block
                } else {
                    scalar(value)?
                };
                description = Some(text);
            }
            // Optional metadata is inert here. In particular, these fields
            // cannot declare tools or grant permissions.
            "license" | "compatibility" | "allowed-tools" | "metadata" | "delegation" => {
                while fields.next_if(|line| line.starts_with(' ')).is_some() {}
            }
            _ => return None,
        }
    }
    let skill = Skill {
        name: name?,
        description: description?,
        sha256: String::new(),
    };
    (skill.name == folder && skill.valid_metadata()).then_some(skill)
}

impl DirectorySkills {
    pub(super) fn discover(
        directory: &Path,
        excluded: impl Fn(&str) -> bool,
        mut pin: impl FnMut(&str, &mut BufReader<File>, u64) -> Result<()>,
    ) -> Result<(Self, Option<String>)> {
        let config = match fs::private_directory(directory) {
            Ok(dir) => dir,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok((Self::default(), None));
            }
            Err(err) if err.kind() == io::ErrorKind::Other => {
                // An existing configuration directory without skills needs no
                // new permission check. Never load skills from this fallback.
                let config = fs::open_nofollow(directory, local::read_flags())
                    .map_err(|_| "skill_directory_unavailable")?;
                if matches!(local::entry_exists(&config, "skills"), Ok(false)) {
                    return Ok((Self::default(), None));
                }
                return Err("skill_directory_unavailable");
            }
            Err(_) => return Err("skill_directory_unavailable"),
        };
        let root = match fs::open_relative_directory(&config, Path::new("skills")) {
            Ok(dir) => dir,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok((Self::default(), None));
            }
            Err(_) => return Err("skill_directory_unavailable"),
        };
        let owner = config
            .metadata()
            .map_err(|_| "skill_directory_unavailable")?
            .uid();
        let private = |dir: &File| {
            dir.metadata()
                .is_ok_and(|m| m.uid() == owner && m.mode() & 0o022 == 0)
        };
        if !private(&root) {
            return Err("skill_directory_unavailable");
        }
        let mut skills = Vec::new();
        let mut index_size = 512;
        let mut warning = None;
        local::visit_names(&root, |entry| {
            let Some(name) = entry
                .to_str()
                .filter(|name| valid_name(name) && !excluded(name))
            else {
                return Ok(());
            };
            let mut load = || {
                let Ok(folder) = fs::open_relative_directory(&root, Path::new(name)) else {
                    return Ok(None);
                };
                if !private(&folder) {
                    return Err("skill_directory_permissions");
                }
                let Some(file) = local::open_private_read(&folder, "SKILL.md")
                    .map_err(|_| "skill_file_unavailable")?
                else {
                    return Ok(None);
                };
                let original = BodyStamp::read(&file).map_err(|_| "skill_file_unavailable")?;
                let mut source = BufReader::new(file);
                let header = read_header(&mut source)?;
                let mut skill = parse_header(name, &header).ok_or("skill_header_invalid")?;
                let start = source
                    .stream_position()
                    .map_err(|_| "skill_file_unavailable")?;
                // Reserve the digest space before reading the body or publishing it.
                let size = serde_json::to_vec(&skill)
                    .map_err(|_| "skill_snapshot_invalid")?
                    .len()
                    + 65;
                if size > MAX_INDEX - index_size {
                    return Err("skill_catalog_metadata_limit");
                }
                let (digest, stamp) =
                    scan_file(&mut source, start, |_| Ok(())).map_err(|_| "skill_body_invalid")?;
                if original != stamp {
                    return Err("skill_source_changed");
                }
                skill.sha256 = digest;
                pin(&skill.sha256, &mut source, start)?;
                Ok(Some((skill, size)))
            };
            match load() {
                Ok(Some((skill, size))) => {
                    index_size += size;
                    skills.push(skill);
                }
                Ok(None) => {}
                Err(code) => {
                    warning.get_or_insert_with(|| format!("{name}: {code}"));
                }
            }
            Ok(())
        })
        .map_err(|_| "skill_directory_scan")?;
        skills.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok((
            Self {
                entries: skills,
                ..Self::default()
            },
            warning,
        ))
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.is_empty()
            || self.entries.iter().any(|skill| !skill.valid())
            || self
                .entries
                .windows(2)
                .any(|pair| pair[0].name >= pair[1].name)
        {
            return Err("skill_snapshot_invalid");
        }
        Ok(())
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|skill| (skill.name.as_str(), skill.description.as_str()))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn list(&self) -> impl Iterator<Item = serde_json::Value> + '_ {
        self.entries.iter().map(|skill| {
            serde_json::json!({"name":skill.name,"description":skill.description,
                "sha256":skill.sha256})
        })
    }

    pub(super) fn read(&self, name: &str, offset: Option<usize>) -> Result<serde_json::Value> {
        let skill = self
            .entries
            .iter()
            .find(|skill| skill.name == name)
            .ok_or("harness_skill_unknown")?;
        let start = offset.unwrap_or_default();
        let limit = if offset.is_some() {
            PAGE_BYTES
        } else {
            INLINE_BODY
        };
        let (mut body, total) = if let Some(dir) = &self.directory {
            let file = local::open_private_read(dir, &format!("body-{}", skill.sha256))
                .map_err(|_| "harness_skill_blob")?
                .ok_or("harness_skill_missing")?;
            self.read_blob(file, &skill.sha256, start, limit)?
        } else {
            let text = self
                .legacy_bodies
                .get(&skill.sha256)
                .ok_or("harness_skill_missing")?;
            let mut body = String::new();
            append_text(
                &mut body,
                text.get(start..).ok_or("harness_skill_offset")?,
                limit,
            );
            (body, text.len())
        };
        let paged = offset.is_some() || total > INLINE_BODY;
        if paged {
            body.truncate(body.floor_char_boundary(PAGE_BYTES));
        }
        let mut value = serde_json::json!({"name":skill.name,"description":skill.description,
            "sha256":skill.sha256,"body":body});
        if paged {
            let next = start + body.len();
            value["offset"] = serde_json::json!(start);
            value["next_offset"] = serde_json::json!((next < total).then_some(next));
        }
        Ok(value)
    }

    fn read_blob(
        &self,
        file: File,
        digest: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(String, usize)> {
        let stamp = BodyStamp::read(&file).map_err(|_| "harness_skill_blob")?;
        let cached = self
            .verified
            .lock()
            .map_err(|_| "harness_skill_blob")?
            .get(digest)
            .copied();
        if cached != Some(stamp) {
            let input = file.try_clone().map_err(|_| "harness_skill_blob")?;
            let (actual, verified) = scan_file(&mut BufReader::new(input), 0, |_| Ok(()))
                .map_err(|_| "harness_skill_body")?;
            if actual != digest || stamp != verified {
                return Err("harness_skill_digest");
            }
            self.verified
                .lock()
                .map_err(|_| "harness_skill_blob")?
                .insert(digest.to_owned(), stamp);
        }
        let total = usize::try_from(stamp.len).map_err(|_| "harness_skill_body")?;
        let remaining = total.checked_sub(offset).ok_or("harness_skill_offset")?;
        let mut bytes = vec![0; remaining.min(limit)];
        file.read_exact_at(&mut bytes, offset as u64)
            .map_err(|_| "harness_skill_blob")?;
        if BodyStamp::read(&file).map_err(|_| "harness_skill_blob")? != stamp {
            return Err("harness_skill_digest");
        }
        if let Err(error) = std::str::from_utf8(&bytes) {
            if error.error_len().is_some() || remaining <= limit {
                return Err("harness_skill_offset");
            }
            bytes.truncate(error.valid_up_to());
        }
        Ok((
            String::from_utf8(bytes).map_err(|_| "harness_skill_body")?,
            total,
        ))
    }
}

pub(super) fn append_text(body: &mut String, text: &str, limit: usize) {
    body.push_str(&text[..text.floor_char_boundary(limit - body.len())]);
}
