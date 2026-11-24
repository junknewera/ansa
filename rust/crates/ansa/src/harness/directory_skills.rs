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
const MAX_SNAPSHOT: usize = 64 * 1024;

#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct DirectorySkills(Vec<Skill>);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Skill {
    name: String,
    description: String,
    body: String,
}

/// A private immutable input referenced by the existing harness epoch pin.
/// Kept separate from adoptable profile manifests and never an active revision.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    version: u8,
    scope: String,
    pub(super) profile: Option<String>,
    pub(super) skills: DirectorySkills,
}

impl Snapshot {
    pub(super) fn new(scope: &str, profile: Option<String>, skills: DirectorySkills) -> Self {
        Self {
            version: 1,
            scope: scope.into(),
            profile,
            skills,
        }
    }

    pub(super) fn publish(&self, dir: &File) -> Result<String> {
        self.skills.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| "skill_snapshot_invalid")?;
        if bytes.len() > MAX_SNAPSHOT {
            return Err("skill_snapshot_size_limit");
        }
        let revision = sha256(&bytes);
        let _lock = local::lock(dir).map_err(|_| "skill_snapshot_busy")?;
        local::publish_recovered(dir, &format!("catalog-{revision}"), &bytes)
            .map_err(|_| "skill_snapshot_publish")?;
        Ok(revision)
    }

    pub(super) fn load(dir: &File, scope: &str, revision: &str) -> Result<Option<Self>> {
        if !digest_ok(revision) {
            return Err("harness_revision");
        }
        let Some(bytes) = local::read(dir, &format!("catalog-{revision}"), MAX_SNAPSHOT)
            .map_err(|_| "skill_snapshot_read")?
        else {
            return Ok(None);
        };
        if sha256(&bytes) != revision {
            return Err("skill_snapshot_digest");
        }
        let snapshot: Self =
            serde_json::from_slice(&bytes).map_err(|_| "skill_snapshot_invalid")?;
        if snapshot.version != 1
            || snapshot.scope != scope
            || snapshot
                .profile
                .as_deref()
                .is_some_and(|profile| !digest_ok(profile))
        {
            return Err("skill_snapshot_invalid");
        }
        snapshot.skills.validate()?;
        Ok(Some(snapshot))
    }
}

impl Skill {
    fn valid(&self) -> bool {
        valid_name(&self.name)
            && !self.description.trim().is_empty()
            && self.description.len() <= MAX_DESCRIPTION
            && !self.description.chars().any(char::is_control)
            && !self.body.trim().is_empty()
            && self.body.len() <= MAX_FILE
            && self
                .body
                .chars()
                .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
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

fn parse(folder: &str, source: &str) -> Option<Skill> {
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
        body: body.to_owned(),
    };
    (skill.name == folder && skill.valid()).then_some(skill)
}

impl DirectorySkills {
    pub(super) fn discover(directory: &Path, excluded: impl Fn(&str) -> bool) -> Result<Self> {
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
                && let Some(skill) = parse(name, text)
            {
                skills.push(skill);
                if skills.len() > MAX_SKILLS {
                    return Err("skill_directory_count_limit");
                }
            }
        }
        skills.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(Self(skills))
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.is_empty()
            || self.0.len() > MAX_SKILLS
            || self.0.iter().any(|skill| !skill.valid())
            || self.0.windows(2).any(|pair| pair[0].name >= pair[1].name)
        {
            return Err("skill_snapshot_invalid");
        }
        Ok(())
    }

    pub(super) fn body(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|skill| skill.name == name)
            .map(|skill| skill.body.as_str())
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|skill| (skill.name.as_str(), skill.description.as_str()))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn list(&self) -> impl Iterator<Item = serde_json::Value> + '_ {
        self.0.iter().map(|skill| {
            serde_json::json!({"name":skill.name,"description":skill.description,
                "sha256":sha256(skill.body.as_bytes())})
        })
    }

    pub(super) fn read(&self, name: &str) -> Result<serde_json::Value> {
        let skill = self
            .0
            .iter()
            .find(|skill| skill.name == name)
            .ok_or("harness_skill_unknown")?;
        Ok(
            serde_json::json!({"name":skill.name,"description":skill.description,
            "sha256":sha256(skill.body.as_bytes()),"body":skill.body}),
        )
    }
}
