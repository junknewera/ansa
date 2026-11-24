use super::registry::Outcome;
use crate::{
    auth::Cancel,
    orchestration::profile::RoleProfile,
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::request::{Advertisement, Descriptor, Request},
};
use std::sync::Arc;

pub(crate) const NAME: &str = "delegate_task";
pub(crate) type Delegate = Arc<
    dyn Fn(&Request, Option<&RoleProfile>, &super::invocation::Invocation, &Cancel) -> Outcome
        + Send
        + Sync,
>;

#[derive(Clone, Copy)]
pub(crate) enum Revision {
    V1,
    V2,
}

impl Revision {
    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "ansa_delegate_v1" => Ok(Self::V1),
            "ansa_delegate_v2" => Ok(Self::V2),
            _ => Err("delegate_revision"),
        }
    }

    pub(crate) fn advertised(tools: &Advertisement) -> Result<Self, &'static str> {
        let descriptor = tools
            .descriptors
            .last()
            .filter(|tool| tool.name == NAME)
            .ok_or("delegate_catalog_conflict")?;
        Self::parse(&descriptor.revision)
    }
}

fn descriptor(revision: Revision) -> Descriptor {
    match revision {
        Revision::V1 => Descriptor {
            name: NAME.into(),
            revision: "ansa_delegate_v1".into(),
            description: "Give one independent task to a helper and receive its final status, summary and findings. Include the context the helper needs. Empty paths allow workspace research. For file changes specify the relative files or directories the helper may read and edit. The lead can run tests itself.".into(),
            schema: Json::parse(r#"{"type":"object","properties":{"task":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}}},"required":["task","paths"],"additionalProperties":false}"#).expect("delegate tool schema"),
        },
        Revision::V2 => Descriptor {
            name: NAME.into(),
            revision: "ansa_delegate_v2".into(),
            description: "Give one independent task to a helper and receive its final status, summary and findings. Include the context the helper needs. Use paths for relative files or directories the helper may read and edit. Use read_paths for additional relative files or directories the helper may only read. Pass [] for an unused list. With both lists empty, the helper can research the workspace without editing. The lead can run tests itself.".into(),
            schema: Json::parse(r#"{"type":"object","properties":{"task":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}},"read_paths":{"type":"array","items":{"type":"string"}}},"required":["task","paths","read_paths"],"additionalProperties":false}"#).expect("delegate tool schema"),
        },
    }
}

pub(crate) struct Assignment<'a> {
    pub(crate) task: &'a str,
    pub(crate) writes: Vec<String>,
    pub(crate) reads: Vec<String>,
}

pub(crate) fn assignment<'a>(
    args: &'a Json,
    parent: &Advertisement,
    revision: Revision,
) -> Result<Assignment<'a>, &'static str> {
    let task = args.text_field("task")?;
    if task.trim().is_empty() || !Json::text_fits_frame(task) {
        return Err("delegate_task_text");
    }
    let paths = |field| -> Result<Vec<_>, _> {
        args.field(field)?
            .elements()?
            .iter()
            .map(|p| p.text().map(str::to_owned))
            .collect()
    };
    let writes = paths("paths")?;
    let has = |name: &str| parent.descriptors.iter().any(|tool| tool.name == name);
    if !["list_directory", "read_file", "search_files"]
        .into_iter()
        .all(has)
        || !writes.is_empty() && !has("edit_file")
    {
        return Err("delegate_parent_access");
    }
    super::files::TaskScope::validate_paths(&writes)?;
    let mut reads = writes.clone();
    if matches!(revision, Revision::V2) {
        let extra = paths("read_paths")?;
        super::files::TaskScope::validate_paths(&extra)?;
        reads.extend(extra);
    }
    Ok(Assignment {
        task,
        writes,
        reads,
    })
}

/// The journal's base catalogue stays immutable. Only an orchestration turn
/// advertises this extra compiled tool, including in pre-existing sessions.
pub(crate) fn advertise(
    base: &Advertisement,
    revision: Revision,
) -> Result<Advertisement, &'static str> {
    let tool = descriptor(revision);
    if base.descriptors.len() >= 64 || base.descriptors.iter().any(|d| d.name == NAME) {
        return Err("delegate_catalog_conflict");
    }
    let mut result = base.clone();
    result.revision = sha256(format!("{}/{}", base.revision, tool.revision).as_bytes());
    result.descriptors.push(tool);
    Ok(result)
}

#[cfg(test)]
#[test]
fn delegated_read_path_stays_read_only_in_the_compiled_tools() {
    use super::{
        files::{TaskScope, local_task_tools, local_tools},
        invocation::Invocation,
    };
    use crate::{
        config::{agent_epoch, load_agent_settings},
        provider::replay::{Effects, Execution},
    };

    let root = std::env::temp_dir().join(format!(
        "ansa-delegate-scope-{}",
        crate::auth::pkce::random_token(16).unwrap()
    ));
    ansa_linux::fs::create_private_leaf(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("report.txt"), "current\n").unwrap();
    std::fs::write(cwd.join("check.sh"), "test fixture\n").unwrap();
    let settings = load_agent_settings(&cwd, &root).unwrap();
    let epoch = agent_epoch(&settings).unwrap();
    let token = sha256(
        epoch
            .exact_state
            .field("instructions")
            .unwrap()
            .dump()
            .as_bytes(),
    );
    let auth = root.join("auth");
    let worker = std::env::current_exe().unwrap();
    let base = local_tools(
        settings.clone(),
        epoch.clone(),
        &root.join("parent"),
        &auth,
        &worker,
    )
    .unwrap();
    let arguments = Json::parse(
        r#"{"task":"Fix the report","paths":["report.txt"],"read_paths":["check.sh"]}"#,
    )
    .unwrap();
    let assigned = assignment(&arguments, base.advertisement(), Revision::V2).unwrap();
    let scope = TaskScope::new(&cwd, &assigned.writes)
        .unwrap()
        .restrict_reads(&assigned.reads)
        .unwrap();
    let tools =
        local_task_tools(settings, epoch, &root.join("child"), &auth, &worker, scope).unwrap();
    for (path, expected) in [("report.txt", "current\n"), ("check.sh", "test fixture\n")] {
        let args = Json::parse(
            &serde_json::json!({"path":path,"offset":0,"max_bytes":128,"instruction_token":token})
                .to_string(),
        )
        .unwrap();
        let outcome = (tools.definition("read_file").unwrap().executor)(&args);
        let output = Json::parse(std::str::from_utf8(outcome.output()).unwrap()).unwrap();
        assert_eq!(output.text_field("content"), Ok(expected));
    }
    let edit = tools.definition("edit_file").unwrap();
    let refused = (tools.policy("edit_file").unwrap().invoke.as_ref().unwrap())(&Invocation {
        operation: "scope:op:1".into(),
        call: "scope-call".into(),
        name: edit.descriptor.name.clone(),
        revision: edit.descriptor.revision.clone(),
        arguments: Json::parse(&serde_json::json!({"path":"check.sh","old_text":"test fixture","new_text":"changed","instruction_token":token}).to_string()).unwrap(),
    }, &Cancel::default());
    assert_eq!(
        (refused.execution(), refused.effects()),
        (Execution::Failed, Effects::KnownResult)
    );
    let output = Json::parse(std::str::from_utf8(refused.output()).unwrap()).unwrap();
    assert_eq!(output.text_field("error"), Ok("task_path_scope"));
    assert!(!output.field("applied").unwrap().boolean().unwrap());
    assert_eq!(
        std::fs::read_to_string(cwd.join("check.sh")).unwrap(),
        "test fixture\n"
    );
    assert!(tools.definition("git_snapshot").is_none());
    std::fs::remove_dir_all(root).unwrap();
}
