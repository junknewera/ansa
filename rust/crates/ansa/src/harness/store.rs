use super::{Result, digest_ok};
use crate::context::prompt_policy::PromptPolicy;
use crate::protocol::legacy_json_v1::sha256;
use ansa_linux::local;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fs::File};

pub(super) const MAX_INPUT: usize = 256 * 1024;
const MAX_MANIFEST: usize = 48 * 1024;
const MAX_SKILL: usize = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fact {
    pub fact: String,
    pub source: String,
    pub applies_when: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillInput {
    name: String,
    trigger: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileInput {
    version: u8,
    #[serde(default)]
    prompt_policy: PromptPolicy,
    guidance: String,
    skills: Vec<SkillInput>,
    memory: Vec<Fact>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Skill {
    pub name: String,
    pub trigger: String,
    pub digest: String,
    pub bytes: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub version: u8,
    #[serde(default, skip_serializing_if = "PromptPolicy::is_legacy")]
    pub prompt_policy: PromptPolicy,
    pub scope: String,
    pub guidance: String,
    pub skills: Vec<Skill>,
    pub memory: Vec<Fact>,
}

fn text_ok(s: &str, max: usize) -> bool {
    !s.trim().is_empty()
        && s.len() <= max
        && !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
}

fn valid_skill_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

impl Manifest {
    fn validate(&self, scope: &str) -> Result<()> {
        if self.version != 1
            || !digest_ok(scope)
            || self.scope != scope
            || self.skills.len() > 32
            || self.memory.len() > 16
            || (!self.guidance.is_empty() && !text_ok(&self.guidance, 4096))
            || self.memory.iter().any(|f| {
                !text_ok(&f.fact, 512) || !text_ok(&f.source, 256) || !text_ok(&f.applies_when, 256)
            })
        {
            return Err("harness_manifest");
        }
        let mut names = BTreeSet::new();
        for skill in &self.skills {
            if !valid_skill_name(&skill.name)
                || !names.insert(&skill.name)
                || !text_ok(&skill.trigger, 256)
                || !digest_ok(&skill.digest)
                || !(1..=MAX_SKILL).contains(&skill.bytes)
            {
                return Err("harness_skill");
            }
        }
        Ok(())
    }
}

pub(super) fn store_manifest(dir: &File, scope: &str, input: &[u8]) -> Result<String> {
    if input.len() > MAX_INPUT {
        return Err("harness_input_limit");
    }
    let profile: ProfileInput =
        serde_json::from_slice(input).map_err(|_| "harness_profile_format")?;
    let mut manifest = Manifest {
        version: profile.version,
        prompt_policy: profile.prompt_policy,
        scope: scope.into(),
        guidance: profile.guidance,
        memory: profile.memory,
        skills: Vec::new(),
    };
    for skill in &profile.skills {
        if !text_ok(&skill.body, MAX_SKILL) {
            return Err("harness_skill_body");
        }
        manifest.skills.push(Skill {
            name: skill.name.clone(),
            trigger: skill.trigger.clone(),
            digest: sha256(skill.body.as_bytes()),
            bytes: skill.body.len(),
        });
    }
    manifest.skills.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.validate(scope)?;
    let bytes = serde_json::to_vec(&manifest).map_err(|_| "harness_manifest")?;
    if bytes.len() > MAX_MANIFEST {
        return Err("harness_manifest_limit");
    }
    // Each immutable file is synced before the revision can be referenced by adoption.
    for skill in profile.skills {
        let bytes = skill.body.as_bytes();
        local::publish_recovered(dir, &format!("skill-{}", sha256(bytes)), bytes)
            .map_err(|_| "harness_blob_conflict")?;
    }
    let revision = sha256(&bytes);
    local::publish_recovered(dir, &format!("profile-{revision}"), &bytes)
        .map_err(|_| "harness_manifest_conflict")?;
    Ok(revision)
}

pub(super) fn empty_manifest(scope: &str) -> Manifest {
    Manifest {
        version: 1,
        prompt_policy: PromptPolicy::default(),
        scope: scope.into(),
        guidance: String::new(),
        skills: Vec::new(),
        memory: Vec::new(),
    }
}

pub(super) fn load_or_empty(dir: &File, scope: &str, revision: Option<&str>) -> Result<Manifest> {
    match revision {
        Some(revision) => load_manifest(dir, scope, revision),
        None => Ok(empty_manifest(scope)),
    }
}

pub(super) fn load_manifest(dir: &File, scope: &str, revision: &str) -> Result<Manifest> {
    if !digest_ok(revision) {
        return Err("harness_revision");
    }
    let bytes = local::read(dir, &format!("profile-{revision}"), MAX_MANIFEST)
        .map_err(|_| "harness_manifest")?
        .ok_or("harness_revision_missing")?;
    if sha256(&bytes) != revision {
        return Err("harness_manifest_digest");
    }
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| "harness_manifest")?;
    manifest.validate(scope)?;
    Ok(manifest)
}

pub(super) fn read_body(dir: &File, skill: &Skill) -> Result<String> {
    let bytes = local::read(dir, &format!("skill-{}", skill.digest), MAX_SKILL)
        .map_err(|_| "harness_skill_blob")?
        .ok_or("harness_skill_missing")?;
    if bytes.len() != skill.bytes || sha256(&bytes) != skill.digest {
        return Err("harness_skill_digest");
    }
    String::from_utf8(bytes).map_err(|_| "harness_skill_body")
}
