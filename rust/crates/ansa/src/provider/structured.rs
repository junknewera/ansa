//! Versioned strict response formats shared by request and response admission.
use crate::{protocol::json::Json, provider::native::NativeItem};
use serde::{Deserialize, Serialize};

type Result<T> = std::result::Result<T, &'static str>;
const MAX_FINDINGS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalFormat {
    ChildResult,
    OrchestrationPlan,
    OrchestrationPlanWithCommands,
    OrchestrationPlanWithReadFolder,
    OrchestrationPlanWithOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    Completed,
    ChangesRequested,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildResult {
    pub status: ChildStatus,
    pub summary: String,
    pub findings: Vec<String>,
}

impl ChildResult {
    pub(crate) fn validate(&self) -> Result<()> {
        if !valid_text(&self.summary)
            || self.findings.len() > MAX_FINDINGS
            || self.findings.iter().any(|finding| !valid_text(finding))
        {
            return Err("child_result_limit");
        }
        Ok(())
    }
}

impl FinalFormat {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ChildResult => "ansa_child_result_v1",
            Self::OrchestrationPlan => "ansa_orchestration_plan_v1",
            Self::OrchestrationPlanWithCommands => "ansa_orchestration_plan_v2",
            Self::OrchestrationPlanWithReadFolder => "ansa_orchestration_plan_v3",
            Self::OrchestrationPlanWithOutcome => "ansa_orchestration_plan_v4",
        }
    }

    pub(crate) fn parse(name: &str) -> Result<Self> {
        match name {
            "ansa_child_result_v1" => Ok(Self::ChildResult),
            "ansa_orchestration_plan_v1" => Ok(Self::OrchestrationPlan),
            "ansa_orchestration_plan_v2" => Ok(Self::OrchestrationPlanWithCommands),
            "ansa_orchestration_plan_v3" => Ok(Self::OrchestrationPlanWithReadFolder),
            "ansa_orchestration_plan_v4" => Ok(Self::OrchestrationPlanWithOutcome),
            _ => Err("unknown_final_format"),
        }
    }

    pub(crate) fn format(self) -> Result<Json> {
        let mut schema = match self {
            Self::ChildResult => Json::parse(
                r#"{"type":"object","properties":{"status":{"type":"string","enum":["completed","changes_requested","blocked"]},"summary":{"type":"string"},"findings":{"type":"array","items":{"type":"string"}}},"required":["status","summary","findings"],"additionalProperties":false}"#,
            )?,
            Self::OrchestrationPlan
            | Self::OrchestrationPlanWithCommands
            | Self::OrchestrationPlanWithReadFolder
            | Self::OrchestrationPlanWithOutcome => Json::parse(
                r#"{"type":"object","properties":{"schema_version":{"type":"integer","enum":[1]},"summary":{"type":"string"},"steps":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"task":{"type":"string"},"access":{"type":"string","enum":["read","write"]},"paths":{"type":"array","items":{"type":"string"}},"depends_on":{"type":"array","items":{"type":"string"}},"acceptance":{"type":"string"}},"required":["id","task","access","paths","depends_on","acceptance"],"additionalProperties":false}},"checks":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"contains":{"type":"string"}},"required":["path","contains"],"additionalProperties":false}},"manual_checks":{"type":"array","items":{"type":"string"}}},"required":["schema_version","summary","steps","checks","manual_checks"],"additionalProperties":false}"#,
            )?,
        };
        if self.has_command_checks() {
            let command_checks = Json::parse(
                r#"{"type":"array","items":{"type":"object","properties":{"argv":{"type":"array","items":{"type":"string"}}},"required":["argv"],"additionalProperties":false}}"#,
            )?;
            schema = require_property(schema, "command_checks", command_checks)?;
        }
        if self.has_read_folder() {
            schema = require_property(
                schema,
                "read_folder",
                Json::parse(r#"{"type":["string","null"]}"#)?,
            )?;
        }
        if self == Self::OrchestrationPlanWithOutcome {
            schema = require_property(
                schema,
                "requested_outcome",
                Json::parse(r#"{"type":"string","enum":["implementation","investigation"]}"#)?,
            )?;
        }
        Json::object(vec![
            ("type", Json::string("json_schema")?),
            ("name", Json::string(self.name())?),
            ("strict", Json::parse("true")?),
            ("schema", schema),
        ])
    }

    pub(super) fn validate(self, items: &[NativeItem]) -> Result<()> {
        match self {
            Self::ChildResult => self.child_result(items).map(|_| ()),
            _ => self.plan(items).map(|_| ()),
        }
    }

    pub(super) fn child_result(self, items: &[NativeItem]) -> Result<ChildResult> {
        if self != Self::ChildResult {
            return Err("child_result_format_required");
        }
        let parsed: ChildResult =
            serde_json::from_str(terminal_text(items)?).map_err(|_| "child_result_schema")?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub(super) fn plan(self, items: &[NativeItem]) -> Result<crate::orchestration::plan::Plan> {
        if self == Self::ChildResult {
            return Err("plan_result_format_required");
        }
        let text = terminal_text(items)?;
        let value = Json::parse(text)?;
        value.field("checks")?.elements()?;
        value.field("manual_checks")?.elements()?;
        if self.has_command_checks() {
            value.field("command_checks")?.elements()?;
        } else if value.has("command_checks") {
            return Err("plan_schema_version");
        }
        if self.has_read_folder() {
            let folder = value.field("read_folder")?;
            if !folder.is_null() {
                folder.text()?;
            }
        } else if value.has("read_folder") {
            return Err("plan_schema_version");
        }
        if (self == Self::OrchestrationPlanWithOutcome) != value.has("requested_outcome") {
            return Err("plan_schema_version");
        }
        if self == Self::OrchestrationPlanWithOutcome {
            value.text_field("requested_outcome")?;
        }
        crate::orchestration::plan::Plan::parse(text)
    }

    pub(crate) fn has_read_folder(self) -> bool {
        matches!(
            self,
            Self::OrchestrationPlanWithReadFolder | Self::OrchestrationPlanWithOutcome
        )
    }

    pub(crate) fn has_command_checks(self) -> bool {
        matches!(
            self,
            Self::OrchestrationPlanWithCommands
                | Self::OrchestrationPlanWithReadFolder
                | Self::OrchestrationPlanWithOutcome
        )
    }
}

fn require_property(schema: Json, name: &str, value: Json) -> Result<Json> {
    let properties = schema.field("properties")?.with(name, value)?;
    let mut required = schema.field("required")?.elements()?.to_vec();
    required.push(Json::string(name)?);
    schema
        .with("properties", properties)?
        .with("required", Json::array(required)?)
}

fn terminal_text(items: &[NativeItem]) -> Result<&str> {
    let mut result = None;
    for item in items {
        let item = item.value();
        if item.text_field("type")? != "message" {
            continue;
        }
        for part in item.field("content")?.elements()? {
            let kind = part.text_field("type")?;
            if kind == "refusal" {
                return Err("child_result_refused");
            }
            if result.is_some() || kind != "output_text" {
                return Err("child_result_shape");
            }
            let text = part.text_field("text")?;
            result = Some(text);
        }
    }
    result.ok_or("child_result_missing")
}

pub(super) fn reject_refusal(items: &[NativeItem]) -> Result<()> {
    for item in items {
        let item = item.value();
        if item.text_field("type")? == "message"
            && item
                .field("content")?
                .elements()?
                .iter()
                .any(|part| part.text_field("type") == Ok("refusal"))
        {
            return Err("child_result_refused");
        }
    }
    Ok(())
}

fn valid_text(text: &str) -> bool {
    !text.trim().is_empty()
        && text
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
}
