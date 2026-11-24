//! Bounded, operator-installed directory skills. Only descriptions go in
//! provider instructions before selection.
use super::{Result, digest_ok};
use crate::protocol::legacy_json_v1::sha256;
use ansa_linux::{fs, local};
use serde::{Deserialize, Serialize};
use std::{fs::File, io, path::Path};

const MAX_ENTRIES: usize = 128;
const MAX_SKILLS: usize = 32;
const MAX_FILE: usize = 16 * 1024;
const MAX_HEADER: usize = 4 * 1024;
const MAX_DESCRIPTION: usize = 512;
// Even the maximum number of fully escaped metadata records fits this bound.
const MAX_INDEX: usize = 512 + MAX_SKILLS * (6 * MAX_DESCRIPTION + 256);
const MAX_LEGACY_SNAPSHOT: usize = 64 * 1024;

#[derive(Default)]
pub(super) struct DirectorySkills {
    entries: Vec<Skill>,
    directory: Option<File>,
    legacy_bodies: std::collections::BTreeMap<String, String>,
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
    use ring::rand::{SecureRandom, SystemRandom};
    let mut random = [0; 16];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| "skill_snapshot_publish")?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    match local::create_immutable(dir, name, data, &suffix) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && existing()? => Ok(()),
        Err(_) => Err("skill_snapshot_publish"),
    }
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

    pub(super) fn pin_body(dir: &File, digest: &str, body: &str) -> Result<()> {
        publish(dir, &format!("body-{digest}"), body.as_bytes())
    }
}

fn valid_body(body: &str) -> bool {
    !body.trim().is_empty()
        && body.len() <= MAX_FILE
        && body
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
}

impl Skill {
    fn valid(&self) -> bool {
        valid_name(&self.name)
            && !self.description.trim().is_empty()
            && self.description.len() <= MAX_DESCRIPTION
            && !self.description.chars().any(char::is_control)
            && digest_ok(&self.sha256)
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

fn parse<'a>(folder: &str, source: &'a str) -> Option<(Skill, &'a str)> {
    let mut lines = source.split_inclusive('\n');
    if !matches!(lines.next(), Some("---\n" | "---\r\n")) {
        return None;
    }
    let mut header_len = 0;
    loop {
        let line = lines.next()?;
        if matches!(line, "---\n" | "---\r\n") {
            break;
        }
        header_len += line.len();
        if header_len > MAX_HEADER {
            return None;
        }
    }
    let start = source.find('\n')? + 1;
    let header = &source[start..start + header_len];
    let body = &source[source.len() - lines.map(str::len).sum::<usize>()..];
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
        sha256: sha256(body.as_bytes()),
    };
    (skill.name == folder && skill.valid() && valid_body(body)).then_some((skill, body))
}

impl DirectorySkills {
    pub(super) fn discover(
        directory: &Path,
        excluded: impl Fn(&str) -> bool,
        mut pin: impl FnMut(&str, &str) -> Result<()>,
    ) -> Result<Self> {
        let config = match fs::private_directory(directory) {
            Ok(dir) => dir,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) if err.kind() == io::ErrorKind::Other => {
                // An existing configuration directory without skills needs no
                // new permission check. Never load skills from this fallback.
                let config = fs::open_nofollow(directory, local::read_flags())
                    .map_err(|_| "skill_directory_unavailable")?;
                if matches!(local::entry_exists(&config, "skills"), Ok(false)) {
                    return Ok(Self::default());
                }
                return Err("skill_directory_unavailable");
            }
            Err(_) => return Err("skill_directory_unavailable"),
        };
        let root = match fs::open_relative_directory(&config, Path::new("skills")) {
            Ok(dir) => dir,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(_) => return Err("skill_directory_unavailable"),
        };
        use std::os::unix::fs::MetadataExt;
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
        let (entries, overflow) =
            local::names(&root, MAX_ENTRIES).map_err(|_| "skill_directory_scan")?;
        if overflow {
            return Err("skill_directory_entry_limit");
        }
        let mut skills = Vec::new();
        for name in entries.iter().filter_map(|entry| entry.to_str()) {
            if !valid_name(name) || excluded(name) {
                continue;
            }
            let Ok(folder) = fs::open_relative_directory(&root, Path::new(name)) else {
                continue;
            };
            if !private(&folder) {
                continue;
            }
            let Ok(Some(bytes)) = local::read(&folder, "SKILL.md", MAX_FILE) else {
                continue;
            };
            if let Ok(text) = std::str::from_utf8(&bytes)
                && let Some((skill, body)) = parse(name, text)
            {
                if skills.len() == MAX_SKILLS {
                    return Err("skill_directory_count_limit");
                }
                pin(&skill.sha256, body)?;
                skills.push(skill);
            }
        }
        skills.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            entries: skills,
            ..Self::default()
        })
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.is_empty()
            || self.entries.len() > MAX_SKILLS
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

    pub(super) fn read(&self, name: &str) -> Result<serde_json::Value> {
        let skill = self
            .entries
            .iter()
            .find(|skill| skill.name == name)
            .ok_or("harness_skill_unknown")?;
        let text;
        let body = if let Some(dir) = &self.directory {
            let bytes = local::read(dir, &format!("body-{}", skill.sha256), MAX_FILE)
                .map_err(|_| "harness_skill_blob")?
                .ok_or("harness_skill_missing")?;
            if sha256(&bytes) != skill.sha256 {
                return Err("harness_skill_digest");
            }
            text = String::from_utf8(bytes).map_err(|_| "harness_skill_body")?;
            if !valid_body(&text) {
                return Err("harness_skill_body");
            }
            &text
        } else {
            self.legacy_bodies
                .get(&skill.sha256)
                .ok_or("harness_skill_missing")?
        };
        Ok(
            serde_json::json!({"name":skill.name,"description":skill.description,
            "sha256":skill.sha256,"body":body}),
        )
    }
}
