//! Immutable provider binding, context epoch, and advertised tool snapshot.
use crate::protocol::json::Json;
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub provider: String,
    pub account: String,
    pub requested_model: String,
    pub returned_model: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Epoch {
    pub id: String,
    pub instructions: String,
    pub exact_state: Json,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Descriptor {
    pub name: String,
    pub description: String,
    pub revision: String,
    pub schema: Json,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Advertisement {
    pub revision: String,
    pub descriptors: Vec<Descriptor>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub binding: Binding,
    pub epoch: Epoch,
    pub tools: Advertisement,
    pub input: Vec<Json>,
}
impl Request {
    pub const MAX_ITEMS: usize = 4096;
    pub fn new(
        binding: Binding,
        epoch: Epoch,
        tools: Advertisement,
        input: Vec<Json>,
    ) -> Result<Self, &'static str> {
        if binding.provider != "openai-siwc"
            || binding.requested_model.is_empty()
            || binding.requested_model.len() > 256
            || binding.account.is_empty()
            || binding.account.len() > 256
            || binding.returned_model.is_empty()
            || binding.returned_model.len() > 256
            || epoch.id.is_empty()
            || epoch.id.len() > 256
            || !epoch.exact_state.is_object()
            || tools.revision.is_empty()
            || tools.descriptors.len() > 64
            || input.len() > Self::MAX_ITEMS
        {
            return Err("request_context");
        }
        for item in &input {
            if !item.is_object() {
                return Err("input_item_object");
            }
            if item.has("role") && item.text_field("role")? == "system" {
                return Err("siwc_system_role");
            }
        }
        Ok(Self {
            binding,
            epoch,
            tools,
            input,
        })
    }
    pub fn json(&self) -> Result<Json, &'static str> {
        let functions = self
            .tools
            .descriptors
            .iter()
            .map(|d| {
                Json::object(vec![
                    ("type", Json::string("function")?),
                    ("name", Json::string(&d.name)?),
                    ("description", Json::string(&d.description)?),
                    ("strict", Json::parse("true")?),
                    ("parameters", d.schema.clone()),
                ])
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tools = if functions.is_empty() {
            Json::array(vec![])?
        } else {
            Json::array(vec![Json::object(vec![
                ("type", Json::string("namespace")?),
                ("name", Json::string("omh")?),
                ("description", Json::string("Compiled local tools")?),
                ("tools", Json::array(functions)?),
            ])?])?
        };
        Json::object(vec![
            ("model", Json::string(&self.binding.requested_model)?),
            ("store", Json::parse("false")?),
            ("stream", Json::parse("true")?),
            ("instructions", Json::string(&self.epoch.instructions)?),
            ("input", Json::array(self.input.clone())?),
            ("tools", tools),
            (
                "include",
                Json::array(vec![Json::string("reasoning.encrypted_content")?])?,
            ),
        ])
    }
    pub(crate) fn fingerprint(&self) -> Result<String, &'static str> {
        let descriptors = self
            .tools
            .descriptors
            .iter()
            .map(|d| {
                Json::object(vec![
                    ("name", Json::string(&d.name)?),
                    ("description", Json::string(&d.description)?),
                    ("implementation_revision", Json::string(&d.revision)?),
                    ("schema", d.schema.clone()),
                ])
            })
            .collect::<Result<Vec<_>, _>>()?;
        let identity = Json::object(vec![
            ("provider", Json::string(&self.binding.provider)?),
            ("account", Json::string(&self.binding.account)?),
            (
                "requested_model",
                Json::string(&self.binding.requested_model)?,
            ),
            (
                "returned_model",
                Json::string(&self.binding.returned_model)?,
            ),
            ("epoch_id", Json::string(&self.epoch.id)?),
            ("instructions", Json::string(&self.epoch.instructions)?),
            ("exact_state", self.epoch.exact_state.clone()),
            ("tools_revision", Json::string(&self.tools.revision)?),
            ("descriptors", Json::array(descriptors)?),
            ("request", self.json()?),
        ])?;
        Ok(crate::protocol::legacy_json_v1::sha256(
            identity.dump().as_bytes(),
        ))
    }
    pub fn save(&self) -> Result<Json, &'static str> {
        let binding = Json::object(vec![
            ("provider", Json::string(&self.binding.provider)?),
            ("account", Json::string(&self.binding.account)?),
            (
                "requested_model",
                Json::string(&self.binding.requested_model)?,
            ),
            (
                "returned_model",
                Json::string(&self.binding.returned_model)?,
            ),
        ])?;
        let epoch = Json::object(vec![
            ("id", Json::string(&self.epoch.id)?),
            ("instructions", Json::string(&self.epoch.instructions)?),
            ("exact_state", self.epoch.exact_state.clone()),
        ])?;
        let descriptors = self
            .tools
            .descriptors
            .iter()
            .map(|d| {
                Json::object(vec![
                    ("name", Json::string(&d.name)?),
                    ("description", Json::string(&d.description)?),
                    ("revision", Json::string(&d.revision)?),
                    ("schema", d.schema.clone()),
                ])
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tools = Json::object(vec![
            ("revision", Json::string(&self.tools.revision)?),
            ("descriptors", Json::array(descriptors)?),
        ])?;
        Json::object(vec![
            ("binding", binding),
            ("epoch", epoch),
            ("tools", tools),
            ("input", Json::array(self.input.clone())?),
        ])
    }
    pub fn load(value: &Json) -> Result<Self, &'static str> {
        let b = value.field("binding")?;
        let e = value.field("epoch")?;
        let t = value.field("tools")?;
        let mut descriptors = Vec::new();
        for d in t.field("descriptors")?.elements()? {
            descriptors.push(Descriptor {
                name: d.text_field("name")?.to_owned(),
                description: d.text_field("description")?.to_owned(),
                revision: d.text_field("revision")?.to_owned(),
                schema: d.field("schema")?.clone(),
            })
        }
        Self::new(
            Binding {
                provider: b.text_field("provider")?.to_owned(),
                account: b.text_field("account")?.to_owned(),
                requested_model: b.text_field("requested_model")?.to_owned(),
                returned_model: b.text_field("returned_model")?.to_owned(),
            },
            Epoch {
                id: e.text_field("id")?.to_owned(),
                instructions: e.text_field("instructions")?.to_owned(),
                exact_state: e.field("exact_state")?.clone(),
            },
            Advertisement {
                revision: t.text_field("revision")?.to_owned(),
                descriptors,
            },
            t_placeholder_input(value)?,
        )
    }
}
fn t_placeholder_input(value: &Json) -> Result<Vec<Json>, &'static str> {
    Ok(value.field("input")?.elements()?.to_vec())
}
