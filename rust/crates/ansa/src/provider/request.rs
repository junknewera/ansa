//! Immutable provider binding, context epoch, and advertised tool snapshot.
use super::structured::FinalFormat;
use crate::protocol::json::Json;

pub(crate) const OPENROUTER_MODEL: &str = "deepseek/deepseek-v4.1-flash-1017";

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
#[derive(Debug, Clone)]
pub struct Request {
    pub binding: Binding,
    pub epoch: Epoch,
    pub tools: Advertisement,
    pub input: Vec<Json>,
    /// Omitted for legacy requests, preserving their exact serialized bytes.
    pub effort: Option<String>,
    final_format: Option<FinalFormat>,
    context_estimate: Option<usize>,
}
impl PartialEq for Request {
    fn eq(&self, other: &Self) -> bool {
        self.binding == other.binding
            && self.epoch == other.epoch
            && self.tools == other.tools
            && self.input == other.input
            && self.effort == other.effort
            && self.final_format == other.final_format
    }
}
pub(crate) fn openrouter_model_id(value: &str) -> bool {
    let Some((author, model)) = value.split_once('/') else {
        return false;
    };
    let component = |s: &str| {
        !s.is_empty()
            && s.len() <= 127
            && s.as_bytes()[0].is_ascii_alphanumeric()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    };
    component(author) && component(model)
}

impl Request {
    pub const MAX_ITEMS: usize = Json::FRAME_NODES;
    pub fn new(
        binding: Binding,
        epoch: Epoch,
        tools: Advertisement,
        input: Vec<Json>,
    ) -> Result<Self, &'static str> {
        let supported = binding.provider == "openai-siwc"
            || binding.provider == "openrouter"
                && openrouter_model_id(&binding.requested_model)
                && binding.returned_model == binding.requested_model
            || cfg!(feature = "orchestration-pilot") && binding.provider == "openrouter-test";
        if !supported
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
            effort: None,
            final_format: None,
            context_estimate: None,
        })
    }
    pub(crate) fn has_image_input(&self) -> bool {
        self.input.iter().any(|item| {
            item.text_field("role") == Ok("user")
                && item
                    .field("content")
                    .is_ok_and(super::contracts::image::has_images)
        })
    }
    pub fn with_effort(mut self, effort: Option<String>) -> Result<Self, &'static str> {
        if effort
            .as_ref()
            .is_some_and(|value| !crate::provider::contracts::valid_id(value))
        {
            return Err("reasoning_effort");
        }
        self.effort = effort;
        Ok(self)
    }
    pub fn with_input(self, input: Vec<Json>) -> Result<Self, &'static str> {
        Ok(Self::new(self.binding, self.epoch, self.tools, input)?
            .with_effort(self.effort)?
            .with_final_format(self.final_format))
    }
    pub fn with_input_bounds(self) -> Result<Self, &'static str> {
        Ok(Self::new(self.binding, self.epoch, self.tools, self.input)?
            .with_effort(self.effort)?
            .with_final_format(self.final_format))
    }
    pub fn with_final_format(mut self, format: Option<FinalFormat>) -> Self {
        self.final_format = format;
        self
    }
    pub fn final_format(&self) -> Option<FinalFormat> {
        self.final_format
    }
    pub(crate) fn set_context_estimate(&mut self, tokens: usize) {
        self.context_estimate = Some(tokens);
    }
    pub(crate) fn estimated_tokens(&self) -> Option<usize> {
        self.context_estimate
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
        let mut value = Json::object(vec![
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
        ])?;
        if let Some(format) = self.final_format {
            value = value.with("text", Json::object(vec![("format", format.format()?)])?)?;
        }
        if let Some(effort) = &self.effort {
            value.with(
                "reasoning",
                Json::object(vec![("effort", Json::string(effort)?)])?,
            )
        } else {
            Ok(value)
        }
    }
    // Keep the newest user item intact and leave room for another attachment
    // batch and continuation metadata. Prior captions and native outputs stay
    // in place so replay and the completed-turn offsets remain meaningful.
    pub(crate) fn archive_history_images(&mut self) -> Result<Vec<usize>, &'static str> {
        use super::contracts::image::{MAX_IMAGE_BYTES, without_archived_images};
        let mut bytes = self
            .input
            .iter()
            .fold(2 + self.input.len().saturating_sub(1), |bytes, item| {
                bytes.saturating_add(item.encoded_bytes())
            });
        let headroom = MAX_IMAGE_BYTES.div_ceil(3) * 4;
        let ceiling = Json::FRAME_BYTES - headroom;
        if !self.has_image_input() {
            return Ok(Vec::new());
        }
        let metadata = self.clone().with_input(Vec::new())?;
        let overhead = metadata
            .identity_value()?
            .encoded_bytes()
            .max(metadata.save()?.encoded_bytes())
            - 2;
        let limit = ceiling.saturating_sub(overhead);
        let end = self.input.len().saturating_sub(1);
        let mut archived = Vec::new();
        for (index, item) in self.input[..end].iter_mut().enumerate() {
            if bytes <= limit {
                break;
            }
            if let Some(text) = without_archived_images(item)?
                && text.encoded_bytes() < item.encoded_bytes()
            {
                bytes -= item.encoded_bytes() - text.encoded_bytes();
                *item = text;
                archived.push(index);
            }
        }
        Ok(archived)
    }

    pub(crate) fn replay_archived_images(&mut self, indexes: &[Json]) -> Result<(), &'static str> {
        use super::contracts::image::without_archived_images;
        if indexes.is_empty() {
            return Err("session_archived_images");
        }
        let end = self.input.len().saturating_sub(1);
        let mut previous = None;
        for entry in indexes {
            let index = entry.index()?;
            if index >= end || previous.is_some_and(|previous| index <= previous) {
                return Err("session_archived_images");
            }
            let item = &mut self.input[index];
            *item = without_archived_images(item)?.ok_or("session_archived_images")?;
            previous = Some(index);
        }
        Ok(())
    }

    fn identity_value(&self) -> Result<Json, &'static str> {
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
        Json::object(vec![
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
        ])
    }
    pub(crate) fn fingerprint(&self) -> Result<String, &'static str> {
        Ok(crate::protocol::legacy_json_v1::sha256(
            self.identity_value()?.dump().as_bytes(),
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
        let mut value = Json::object(vec![
            ("binding", binding),
            ("epoch", epoch),
            ("tools", tools),
            ("input", Json::array(self.input.clone())?),
        ])?;
        if let Some(format) = self.final_format {
            value = value.with("final_format", Json::string(format.name())?)?;
        }
        if let Some(effort) = &self.effort {
            value.with("effort", Json::string(effort)?)
        } else {
            Ok(value)
        }
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
        let request = Self::new(
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
        )?
        .with_effort(if value.has("effort") {
            Some(value.text_field("effort")?.into())
        } else {
            None
        })?;
        let format = if value.has("final_format") {
            Some(FinalFormat::parse(value.text_field("final_format")?)?)
        } else {
            None
        };
        Ok(request.with_final_format(format))
    }
}
fn t_placeholder_input(value: &Json) -> Result<Vec<Json>, &'static str> {
    Ok(value.field("input")?.elements()?.to_vec())
}
