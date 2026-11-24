//! Immutable tool definition snapshots.
use crate::{
    protocol::json::Json,
    provider::{
        replay::{Effects, Execution},
        request::{Advertisement, Descriptor},
    },
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
pub type Validator = Arc<dyn Fn(&Json) -> bool + Send + Sync>;
pub type ArgumentHint = Arc<dyn Fn(&Json) -> Option<&'static str> + Send + Sync>;
pub type Executor = Arc<dyn Fn(&Json) -> Outcome + Send + Sync>;
pub type ContextExecutor =
    Arc<dyn Fn(&super::invocation::Invocation, &crate::auth::Cancel) -> Outcome + Send + Sync>;
pub type Recovery = Arc<
    dyn Fn(&super::invocation::Invocation) -> Result<Option<Outcome>, &'static str> + Send + Sync,
>;
#[derive(Clone, Default)]
pub struct Policy {
    pub argument_hint: Option<ArgumentHint>,
    pub invoke: Option<ContextExecutor>,
    pub recover: Option<Recovery>,
    pub requires_journal: bool,
    pub read_only: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub(crate) execution: Execution,
    pub(crate) effects: Effects,
    pub(crate) output: Vec<u8>,
}
impl Outcome {
    pub fn from_bytes(execution: Execution, effects: Effects, output: Vec<u8>) -> Self {
        Self {
            execution,
            effects,
            output,
        }
    }
    pub fn succeeded(output: String) -> Self {
        Self {
            execution: Execution::Succeeded,
            effects: Effects::KnownResult,
            output: output.into_bytes(),
        }
    }
    pub fn failed_known(output: String) -> Self {
        Self {
            execution: Execution::Failed,
            effects: Effects::KnownResult,
            output: output.into_bytes(),
        }
    }
    pub fn failed_unknown() -> Self {
        Self {
            execution: Execution::Failed,
            effects: Effects::Unknown,
            output: Vec::new(),
        }
    }
    pub fn not_started() -> Self {
        Self {
            execution: Execution::Cancelled,
            effects: Effects::NotStarted,
            output: Vec::new(),
        }
    }
    pub fn effects(&self) -> Effects {
        self.effects
    }
    pub fn execution(&self) -> Execution {
        self.execution
    }
    pub fn output(&self) -> &[u8] {
        &self.output
    }
}
#[derive(Clone)]
pub struct Definition {
    pub descriptor: Descriptor,
    pub validator: Validator,
    pub executor: Executor,
}
pub struct Registry {
    definitions: Vec<Definition>,
    policies: Vec<Policy>,
    records: Vec<Json>,
    owner: Option<BoundOwner>,
}
#[derive(Clone)]
struct BoundOwner {
    session: String,
    directory: PathBuf,
    signal_cancel: Arc<dyn Fn() + Send + Sync>,
    plan_ready: Arc<dyn Fn() -> bool + Send + Sync>,
    begin_turn: Arc<dyn Fn() + Send + Sync>,
}
/// Snapshot provenance stays immutable even if an Arc is uniquely owned.
/// ```compile_fail,E0616
/// use ansa::tools::registry::Snapshot;
/// fn rewrite(snapshot: &mut Snapshot) {
///     snapshot.advertisement = todo!();
/// }
/// ```
pub struct Snapshot {
    pub(crate) definitions: Vec<Definition>,
    advertisement: Advertisement,
    policies: Vec<Policy>,
    owner: Option<BoundOwner>,
}
impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            definitions: vec![],
            policies: vec![],
            records: vec![],
            owner: None,
        }
    }

    pub(crate) fn bind_owner(
        &mut self,
        session: &str,
        directory: &Path,
        signal_cancel: Arc<dyn Fn() + Send + Sync>,
        plan_ready: Arc<dyn Fn() -> bool + Send + Sync>,
        begin_turn: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), &'static str> {
        if !crate::provider::contracts::valid_id(session)
            || !directory.is_absolute()
            || ansa_linux::fs::normalize(directory) != directory
            || self.owner.is_some()
        {
            return Err("tool_owner_binding");
        }
        self.owner = Some(BoundOwner {
            session: session.to_owned(),
            directory: directory.to_path_buf(),
            signal_cancel,
            plan_ready,
            begin_turn,
        });
        Ok(())
    }
    pub fn register(&mut self, d: Definition) -> Result<(), &'static str> {
        self.register_with_policy(d, Policy::default())
    }
    pub fn register_with_policy(
        &mut self,
        d: Definition,
        policy: Policy,
    ) -> Result<(), &'static str> {
        if self.definitions.len() >= 64
            || d.descriptor.name.is_empty()
            || d.descriptor.name.len() > 64
            || !d
                .descriptor
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || d.descriptor.revision.is_empty()
            || d.descriptor.revision.len() > 128
            || self
                .definitions
                .iter()
                .any(|x| x.descriptor.name == d.descriptor.name)
        {
            return Err("invalid_tool_registration");
        };
        super::schema::validate_schema(&d.descriptor.schema, 0)?;
        if d.descriptor.schema.field("type")?.text()? != "object" {
            return Err("tool_schema_root");
        };
        let descriptor = Json::object(vec![
            ("name", Json::string(&d.descriptor.name)?),
            ("description", Json::string(&d.descriptor.description)?),
            (
                "implementation_revision",
                Json::string(&d.descriptor.revision)?,
            ),
            ("schema", d.descriptor.schema.clone()),
        ])?;
        let record = if policy.requires_journal {
            Json::object(vec![
                ("descriptor", descriptor),
                ("requires_journal", Json::parse("true")?),
            ])?
        } else {
            descriptor
        };
        self.records.push(record);
        if let Err(code) = Json::array(self.records.clone()) {
            self.records.pop();
            return Err(code);
        }
        self.definitions.push(d);
        self.policies.push(policy);
        Ok(())
    }
    pub fn snapshot(&self) -> Arc<Snapshot> {
        let descriptors = self
            .definitions
            .iter()
            .map(|d| d.descriptor.clone())
            .collect();
        let serialized = Json::array(self.records.clone())
            .expect("bounded registry")
            .dump();
        let revision = crate::protocol::legacy_json_v1::sha256(serialized.as_bytes());
        Arc::new(Snapshot {
            definitions: self.definitions.clone(),
            policies: self.policies.clone(),
            owner: self.owner.clone(),
            advertisement: Advertisement {
                revision,
                descriptors,
            },
        })
    }
}
impl Snapshot {
    pub(crate) fn for_lead(
        &self,
        revision: super::delegation::Revision,
        invoke: ContextExecutor,
    ) -> Result<Arc<Self>, &'static str> {
        let advertisement = super::delegation::advertise(&self.advertisement, revision)?;
        let base = self.advertisement.clone();
        let mut definitions = self.definitions.clone();
        definitions.push(Definition {
            descriptor: advertisement
                .descriptors
                .last()
                .expect("added delegate")
                .clone(),
            validator: Arc::new(move |args| {
                super::delegation::assignment(args, &base, revision).is_ok()
            }),
            executor: Arc::new(|_| Outcome::not_started()),
        });
        let mut policies = self.policies.clone();
        policies.push(Policy {
            invoke: Some(invoke),
            requires_journal: true,
            ..Policy::default()
        });
        Ok(Arc::new(Self {
            advertisement,
            definitions,
            policies,
            owner: self.owner.clone(),
        }))
    }

    /// Retain the exact compiled subset and order of a saved advertisement.
    /// Saved descriptions never supply executable definitions or policies.
    /// Recomputing the revision also checks the saved durable-intent policy.
    pub fn compatible(&self, saved: &Advertisement) -> Option<Arc<Self>> {
        if !self.definitions_match_advertisement() {
            return None;
        }
        let mut registry = Registry::new();
        registry.owner = self.owner.clone();
        for descriptor in &saved.descriptors {
            let index = self
                .definitions
                .iter()
                .position(|definition| definition.descriptor.name == descriptor.name)?;
            let mut definition = self.definitions[index].clone();
            if definition.descriptor != *descriptor {
                let legacy =
                    crate::process::output_spool::legacy_descriptor(&definition.descriptor)
                        .or_else(|| super::edit::legacy_descriptor(&definition.descriptor))
                        .filter(|legacy| legacy == descriptor)?;
                definition.descriptor = legacy;
            }
            registry
                .register_with_policy(definition, self.policies[index].clone())
                .ok()?;
        }
        let subset = registry.snapshot();
        (subset.advertisement == *saved).then_some(subset)
    }
    pub fn requires_journal(&self) -> bool {
        self.policies.iter().any(|p| p.requires_journal)
    }
    pub fn read_only(&self) -> Arc<Self> {
        self.filtered(|_, policy| policy.read_only)
    }

    pub(crate) fn verified_plan_tools(&self, saved: &Advertisement) -> Option<Advertisement> {
        let mut registry = Registry::new();
        for descriptor in &saved.descriptors {
            let Some(index) = self
                .definitions
                .iter()
                .position(|d| d.descriptor.name == descriptor.name)
            else {
                continue;
            };
            if !self.policies[index].read_only {
                continue;
            }
            if self.definitions[index].descriptor != *descriptor {
                return None;
            }
            registry
                .register_with_policy(
                    self.definitions[index].clone(),
                    self.policies[index].clone(),
                )
                .ok()?;
        }
        Some(registry.snapshot().advertisement.clone())
    }

    pub(crate) fn named_tools(&self, names: &[&str]) -> Arc<Self> {
        self.filtered(|definition, _| names.contains(&definition.descriptor.name.as_str()))
    }

    pub(crate) fn without_tool(&self, name: &str) -> Arc<Self> {
        self.filtered(|definition, _| definition.descriptor.name != name)
    }

    fn filtered(&self, include: impl Fn(&Definition, &Policy) -> bool) -> Arc<Self> {
        let mut registry = Registry::new();
        registry.owner = self.owner.clone();
        for (definition, policy) in self.definitions.iter().zip(&self.policies) {
            if include(definition, policy) {
                registry
                    .register_with_policy(definition.clone(), policy.clone())
                    .expect("subset of validated registry");
            }
        }
        registry.snapshot()
    }
    pub(crate) fn policy(&self, name: &str) -> Option<&Policy> {
        self.definitions
            .iter()
            .position(|d| d.descriptor.name == name)
            .and_then(|index| self.policies.get(index))
    }
    pub fn advertisement(&self) -> &Advertisement {
        &self.advertisement
    }

    pub(crate) fn owner_matches(&self, session: &str, directory: Option<&Path>) -> bool {
        self.owner.as_ref().is_none_or(|owner| {
            owner.session == session && Some(owner.directory.as_path()) == directory
        })
    }

    pub(crate) fn signal_cancel(&self) {
        if let Some(owner) = &self.owner {
            (owner.signal_cancel)();
        }
    }

    pub(crate) fn plan_ready(&self) -> bool {
        self.owner.as_ref().is_none_or(|owner| (owner.plan_ready)())
    }

    pub(crate) fn begin_turn(&self) {
        if let Some(owner) = &self.owner {
            (owner.begin_turn)();
        }
    }

    pub(crate) fn definitions_match_advertisement(&self) -> bool {
        self.definitions.len() == self.policies.len()
            && self.definitions.len() == self.advertisement.descriptors.len()
            && self
                .definitions
                .iter()
                .map(|definition| &definition.descriptor)
                .eq(self.advertisement.descriptors.iter())
    }

    pub fn definition(&self, name: &str) -> Option<&Definition> {
        self.definitions.iter().find(|d| d.descriptor.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definition(name: &str) -> Definition {
        let output = name.to_owned();
        Definition {
            descriptor: Descriptor { name: name.into(), description: format!("Compiled {name}"),
                revision: "revision-1".into(), schema: Json::parse(r#"{"type":"object","properties":{},"required":[],"additionalProperties":false}"#).unwrap() },
            validator: Arc::new(|_| true),
            executor: Arc::new(move |_| Outcome::succeeded(output.clone())),
        }
    }
    fn registered(names: &[&str]) -> Arc<Snapshot> {
        let mut registry = Registry::new();
        for name in names {
            registry
                .register_with_policy(
                    definition(name),
                    Policy {
                        requires_journal: *name != "read",
                        read_only: *name == "read",
                        ..Policy::default()
                    },
                )
                .unwrap();
        }
        registry.snapshot()
    }
    #[test]
    fn delegate_admission_respects_parent_tools_and_relative_scope() {
        let read = ["list_directory", "read_file", "search_files"];
        for writable in [false, true] {
            let mut names = read.to_vec();
            if writable {
                names.push("edit_file");
            }
            let tools = registered(&names)
                .for_lead(
                    super::super::delegation::Revision::V1,
                    Arc::new(|_, _| panic!("admission dispatched a helper")),
                )
                .unwrap();
            let valid = &tools
                .definition(super::super::delegation::NAME)
                .unwrap()
                .validator;
            for (paths, allowed) in [
                ("[]", true),
                (r#"["src/lib.rs"]"#, writable),
                (r#"["../outside"]"#, false),
                (r#"["/tmp/write"]"#, false),
                (r#"[".git/config"]"#, false),
            ] {
                let args =
                    Json::parse(&format!(r#"{{"task":"Read or edit","paths":{paths}}}"#)).unwrap();
                assert_eq!(valid(&args), allowed, "{paths}");
            }
        }
    }
    #[test]
    fn delegation_revisions_keep_distinct_strict_arguments() {
        use super::super::delegation::{self, Revision};
        let parent = registered(&["list_directory", "read_file", "search_files", "edit_file"]);
        let legacy = Json::parse(r#"{"task":"Fix report","paths":["src/report.rs"]}"#).unwrap();
        let current = legacy
            .with("read_paths", Json::parse(r#"["tests/report.rs"]"#).unwrap())
            .unwrap();
        for (revision, accepts, rejects) in [
            (Revision::V1, &legacy, &current),
            (Revision::V2, &current, &legacy),
        ] {
            let advertised = delegation::advertise(parent.advertisement(), revision).unwrap();
            let descriptor = advertised.descriptors.last().unwrap();
            super::super::schema::validate_schema(&descriptor.schema, 0).unwrap();
            assert!(super::super::schema::matches(&descriptor.schema, accepts));
            assert!(!super::super::schema::matches(&descriptor.schema, rejects));
            let assignment = delegation::assignment(accepts, &advertised, revision).unwrap();
            assert_eq!(assignment.writes, ["src/report.rs"]);
            if matches!(revision, Revision::V2) {
                assert_eq!(assignment.reads, ["src/report.rs", "tests/report.rs"]);
            } else {
                assert_eq!(assignment.reads, ["src/report.rs"]);
            }
        }
        let readonly = registered(&["list_directory", "read_file", "search_files"]);
        let args = current.with("paths", Json::parse("[]").unwrap()).unwrap();
        let assignment =
            delegation::assignment(&args, readonly.advertisement(), Revision::V2).unwrap();
        assert!(assignment.writes.is_empty());
        assert_eq!(assignment.reads, ["tests/report.rs"]);
        let bad_path = args
            .with("read_paths", Json::parse(r#"["../other"]"#).unwrap())
            .unwrap();
        assert!(delegation::assignment(&bad_path, readonly.advertisement(), Revision::V2).is_err());
    }
    #[test]
    fn compatible_subset_uses_compiled_executors_and_exact_saved_order_and_policies() {
        let current = registered(&["edit", "run", "read"]);
        let old = registered(&["read", "run"]);
        let subset = current.compatible(old.advertisement()).unwrap();
        assert_eq!(subset.advertisement(), old.advertisement());
        assert!(subset.definition("edit").is_none());
        assert!(subset.policy("run").unwrap().requires_journal);
        assert!(subset.policy("read").unwrap().read_only);
        assert_eq!(
            (subset.definition("read").unwrap().executor)(&Json::parse("{}").unwrap()).output(),
            b"read"
        );
        assert_eq!(
            subset.read_only().advertisement(),
            old.read_only().advertisement()
        );
        assert_eq!(current.advertisement().descriptors.len(), 3);
    }
    #[test]
    fn compatible_subset_rejects_changed_unknown_or_duplicate_descriptors() {
        let current = registered(&["read", "run", "edit"]);
        for changed in 0..4 {
            let mut definition = definition("read");
            match changed {
                0 => definition.descriptor.name = "missing".into(),
                1 => definition.descriptor.description.push_str(" changed"),
                2 => definition.descriptor.revision = "revision-2".into(),
                _ => definition.descriptor.schema = Json::parse(r#"{"type":"object","properties":{"new":{"type":"string"}},"required":["new"],"additionalProperties":false}"#).unwrap(),
            }
            let mut registry = Registry::new();
            registry
                .register_with_policy(
                    definition,
                    Policy {
                        read_only: true,
                        ..Policy::default()
                    },
                )
                .unwrap();
            assert!(
                current
                    .compatible(registry.snapshot().advertisement())
                    .is_none()
            );
        }
        let mut duplicate = registered(&["read"]).advertisement().clone();
        duplicate.descriptors.push(duplicate.descriptors[0].clone());
        assert!(current.compatible(&duplicate).is_none());
    }
    #[test]
    fn compatible_subset_checks_revision_and_durable_policy_without_upgrading_readonly() {
        let current = registered(&["read", "run", "edit"]);
        let readonly = current.read_only();
        assert_eq!(
            current
                .compatible(readonly.advertisement())
                .unwrap()
                .advertisement(),
            readonly.advertisement()
        );
        assert!(readonly.compatible(current.advertisement()).is_none());
        let mut forged = readonly.advertisement().clone();
        forged.revision = "forged".into();
        assert!(current.compatible(&forged).is_none());
        let mut wrong_policy = Registry::new();
        wrong_policy.register(definition("run")).unwrap();
        assert!(
            current
                .compatible(wrong_policy.snapshot().advertisement())
                .is_none()
        );
    }
}
