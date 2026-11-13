//! Immutable tool definition snapshots.
use crate::{
    protocol::json::Json,
    provider::{
        replay::{Effects, Execution},
        request::{Advertisement, Descriptor},
    },
};
use std::sync::Arc;
pub type Validator = Arc<dyn Fn(&Json) -> bool + Send + Sync>;
pub type Executor = Arc<dyn Fn(&Json) -> Outcome + Send + Sync>;
pub type ContextExecutor =
    Arc<dyn Fn(&super::invocation::Invocation, &crate::auth::Cancel) -> Outcome + Send + Sync>;
pub type Recovery = Arc<
    dyn Fn(&super::invocation::Invocation) -> Result<Option<Outcome>, &'static str> + Send + Sync,
>;
#[derive(Clone, Default)]
pub struct Policy {
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
        }
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
        let records = self
            .definitions
            .iter()
            .zip(&self.policies)
            .map(|(d, policy)| {
                let descriptor = Json::object(vec![
                    (
                        "name",
                        Json::string(&d.descriptor.name).expect("validated descriptor"),
                    ),
                    (
                        "description",
                        Json::string(&d.descriptor.description).expect("valid descriptor"),
                    ),
                    (
                        "implementation_revision",
                        Json::string(&d.descriptor.revision).expect("validated revision"),
                    ),
                    ("schema", d.descriptor.schema.clone()),
                ])
                .expect("validated descriptor");
                if policy.requires_journal {
                    Json::object(vec![
                        ("descriptor", descriptor),
                        ("requires_journal", Json::parse("true").expect("boolean")),
                    ])
                    .expect("bounded descriptor")
                } else {
                    descriptor
                }
            })
            .collect();
        let serialized = Json::array(records).expect("bounded registry").dump();
        let revision = crate::protocol::legacy_json_v1::sha256(serialized.as_bytes());
        Arc::new(Snapshot {
            definitions: self.definitions.clone(),
            policies: self.policies.clone(),
            advertisement: Advertisement {
                revision,
                descriptors,
            },
        })
    }
}
impl Snapshot {
    pub fn requires_journal(&self) -> bool {
        self.policies.iter().any(|p| p.requires_journal)
    }
    pub fn read_only(&self) -> Arc<Self> {
        let mut registry = Registry::new();
        for (definition, policy) in self.definitions.iter().zip(&self.policies) {
            if policy.read_only {
                registry.definitions.push(definition.clone());
                registry.policies.push(policy.clone());
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
