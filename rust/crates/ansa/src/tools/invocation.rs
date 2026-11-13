//! Runtime-owned invocation identity and the legacy v1 representation.
use crate::protocol::json::Json;
#[derive(Clone, Debug, PartialEq)]
pub struct Invocation {
    pub operation: String,
    pub call: String,
    pub name: String,
    pub revision: String,
    pub arguments: Json,
}
impl Invocation {
    pub fn load(value: &Json) -> Result<Self, &'static str> {
        Ok(Self {
            operation: value.text_field("operation")?.into(),
            call: value.text_field("call")?.into(),
            name: value.text_field("name")?.into(),
            revision: value.text_field("revision")?.into(),
            arguments: value.field("arguments")?.clone(),
        })
    }
    pub fn save(&self) -> Result<Json, &'static str> {
        Json::object(vec![
            ("operation", Json::string(&self.operation)?),
            ("call", Json::string(&self.call)?),
            ("name", Json::string(&self.name)?),
            ("revision", Json::string(&self.revision)?),
            ("arguments", self.arguments.clone()),
        ])
    }
}
