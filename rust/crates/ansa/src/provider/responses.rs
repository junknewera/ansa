//! Responses reducer/admission types. Constructors remain private to this module.
use crate::{
    protocol::{
        json::Json,
        sse::{Event, Limits, Parser},
    },
    provider::{native::NativeItem, request::Request},
};
use std::collections::{BTreeMap, HashSet};
pub(crate) type TextPreview = Box<dyn FnMut(&str)>;
type PreviewPart = (String, String, Option<String>, bool);
type PreviewItem = (Json, Option<Json>, BTreeMap<usize, PreviewPart>);
struct ValidatedResponse {
    id: String,
    model: String,
    items: Vec<NativeItem>,
    calls: Vec<FunctionCall>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCall {
    pub item_id: String,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub output_index: usize,
}
#[derive(Debug)]
/// Admission cannot be manufactured by deserializing peer data or mutating a
/// completed response's private identity.
/// ```compile_fail,E0277
/// use ansa::provider::responses::AdmittedResponse;
/// fn needs_deserialize<T: for<'de> serde::Deserialize<'de>>() {}
/// needs_deserialize::<AdmittedResponse>();
/// ```
/// ```compile_fail,E0616
/// use ansa::provider::responses::AdmittedResponse;
/// fn change_identity(mut response: AdmittedResponse) {
///     response.id = String::from("forged");
/// }
/// ```
pub struct AdmittedResponse {
    pub(crate) request: Request,
    pub(crate) id: String,
    pub(crate) returned_model: String,
    pub(crate) items: Vec<NativeItem>,
    pub(crate) calls: Vec<FunctionCall>,
    pub(crate) receipt: super::replay::AdmissionReceipt,
}
impl AdmittedResponse {
    pub fn request(&self) -> &Request {
        &self.request
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn returned_model(&self) -> &str {
        &self.returned_model
    }
    pub fn items(&self) -> &[NativeItem] {
        &self.items
    }
    pub fn calls(&self) -> &[FunctionCall] {
        &self.calls
    }
}
#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
pub(crate) struct Reducer {
    request: Request,
    events: Vec<Event>,
    poisoned: bool,
    bytes: usize,
    limits: Limits,
    preview: Option<TextPreview>,
    preview_text: String,
}
#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
impl Reducer {
    pub(crate) fn new(request: Request, limits: Limits, preview: Option<TextPreview>) -> Self {
        Self {
            request,
            events: Vec::new(),
            poisoned: false,
            bytes: 0,
            limits,
            preview,
            preview_text: String::new(),
        }
    }
    pub(crate) fn event(&mut self, event: Event) -> Result<(), &'static str> {
        if self.poisoned {
            return Err("reducer_poisoned");
        }
        if event.data.len() > self.limits.event {
            return Err("response_event_limit");
        };
        self.bytes = self
            .bytes
            .checked_add(event.data.len())
            .ok_or("response_body_limit")?;
        if self.bytes > self.limits.body {
            self.poisoned = true;
            return Err("response_body_limit");
        };
        self.events.push(event);
        if self.preview.is_some() {
            // Provisional text still belongs to a validated event prefix.
            // This shares semantic validation with final admission, without
            // constructing an admission receipt before transport EOF.
            if let Err(error) = self.validate_events(true) {
                self.poisoned = true;
                return Err(error);
            }
            self.emit_preview();
        }
        Ok(())
    }
    fn emit_preview(&mut self) {
        let mut items: BTreeMap<usize, PreviewItem> = BTreeMap::new();
        for event in &self.events {
            let Ok(value) = Json::parse(&event.data) else {
                continue;
            };
            let Ok(ty) = value.text_field("type") else {
                continue;
            };
            match ty {
                "response.output_item.added" => {
                    if let (Ok(i), Ok(item)) =
                        (value.usize_field("output_index"), value.field("item"))
                    {
                        items.insert(i, (item.clone(), None, BTreeMap::new()));
                    }
                }
                "response.output_item.done" => {
                    if let (Ok(i), Ok(item)) =
                        (value.usize_field("output_index"), value.field("item"))
                        && let Some(state) = items.get_mut(&i)
                    {
                        state.1 = Some(item.clone())
                    }
                }
                "response.content_part.added" | "response.reasoning_summary_part.added" => {
                    let summary = ty.starts_with("response.reasoning_summary_");
                    if let (Ok(i), Ok(j), Ok(part)) = (
                        value.usize_field("output_index"),
                        value.usize_field(if summary {
                            "summary_index"
                        } else {
                            "content_index"
                        }),
                        value.field("part"),
                    ) && let (Ok(kind), Ok(text)) = (
                        part.text_field("type"),
                        part.text_field(if part.text_field("type") == Ok("refusal") {
                            "refusal"
                        } else {
                            "text"
                        }),
                    ) && let Some(state) = items.get_mut(&i)
                    {
                        state.2.insert(
                            j,
                            (kind.to_owned(), String::new(), Some(text.to_owned()), false),
                        );
                    }
                }
                "response.output_text.delta"
                | "response.refusal.delta"
                | "response.reasoning_summary_text.delta" => {
                    let summary = ty.starts_with("response.reasoning_summary_");
                    if let (Ok(i), Ok(j), Ok(delta)) = (
                        value.usize_field("output_index"),
                        value.usize_field(if summary {
                            "summary_index"
                        } else {
                            "content_index"
                        }),
                        value.text_field("delta"),
                    ) && let Some(part) = items.get_mut(&i).and_then(|s| s.2.get_mut(&j))
                    {
                        part.1.push_str(delta);
                        part.2 = None;
                    }
                }
                "response.output_text.done"
                | "response.refusal.done"
                | "response.reasoning_summary_text.done" => {
                    let summary = ty.starts_with("response.reasoning_summary_");
                    let field = if ty.starts_with("response.refusal.") {
                        "refusal"
                    } else {
                        "text"
                    };
                    if let (Ok(i), Ok(j), Ok(text)) = (
                        value.usize_field("output_index"),
                        value.usize_field(if summary {
                            "summary_index"
                        } else {
                            "content_index"
                        }),
                        value.text_field(field),
                    ) && let Some(part) = items.get_mut(&i).and_then(|s| s.2.get_mut(&j))
                    {
                        part.2 = Some(text.to_owned());
                    }
                }
                "response.content_part.done" | "response.reasoning_summary_part.done" => {
                    let summary = ty.starts_with("response.reasoning_summary_");
                    if let (Ok(i), Ok(j)) = (
                        value.usize_field("output_index"),
                        value.usize_field(if summary {
                            "summary_index"
                        } else {
                            "content_index"
                        }),
                    ) && let Some(part) = items.get_mut(&i).and_then(|s| s.2.get_mut(&j))
                    {
                        part.3 = true;
                    }
                }
                _ => {}
            }
        }
        let mut text = String::new();
        for (_index, (added, done, parts)) in items {
            if let Some(item) = done {
                if item.text_field("type").ok() == Some("message")
                    && let Ok(content) = item.field("content").and_then(Json::elements)
                {
                    for part in content {
                        let value = part
                            .text_field(if part.text_field("type") == Ok("refusal") {
                                "refusal"
                            } else {
                                "text"
                            })
                            .unwrap_or("");
                        if text.len() + value.len() > 64 * 1024 {
                            return;
                        };
                        text.push_str(value)
                    }
                };
                continue;
            };
            if added.text_field("type").ok() == Some("message") {
                for (_, (kind, delta, final_text, closed)) in parts {
                    let _ = kind;
                    let value = final_text.unwrap_or(delta);
                    if text.len() + value.len() > 64 * 1024 {
                        return;
                    };
                    text.push_str(&value);
                    if !closed {
                        break;
                    }
                }
            };
            break;
        }
        if text.len() > self.preview_text.len() && text.starts_with(&self.preview_text) {
            self.preview_text = text.clone();
            if let Some(callback) = &mut self.preview {
                callback(&self.preview_text)
            }
        }
    }
    pub(crate) fn finish(&mut self) -> Result<AdmittedResponse, &'static str> {
        if self.poisoned {
            return Err("reducer_poisoned");
        };
        let result = self.finish_inner();
        if result.is_err() {
            self.poisoned = true
        };
        result
    }
    fn finish_inner(&self) -> Result<AdmittedResponse, &'static str> {
        let value = self
            .validate_events(false)?
            .ok_or("response_premature_eof")?;
        Ok(AdmittedResponse {
            request: self.request.clone(),
            id: value.id,
            returned_model: value.model,
            items: value.items,
            calls: value.calls,
            receipt: super::replay::receipt(&self.request)?,
        })
    }
    fn validate_events(
        &self,
        allow_prefix: bool,
    ) -> Result<Option<ValidatedResponse>, &'static str> {
        if self.events.is_empty() {
            return Err("response_empty");
        };
        let mut response_id = String::new();
        let mut created_model = String::new();
        let mut terminal: Option<Json> = None;
        let mut seq = 0usize;
        let mut added: BTreeMap<usize, Json> = BTreeMap::new();
        let mut done: BTreeMap<usize, Json> = BTreeMap::new();
        let mut arg_delta: BTreeMap<usize, String> = BTreeMap::new();
        let mut arg_final: BTreeMap<usize, String> = BTreeMap::new();
        let mut ids = HashSet::new();
        let mut calls = Vec::new();
        let mut progress = false;
        let mut parts: BTreeMap<(usize, usize), Json> = BTreeMap::new();
        let mut part_done: BTreeMap<(usize, usize), Json> = BTreeMap::new();
        let mut text_delta: BTreeMap<(usize, usize), String> = BTreeMap::new();
        let mut text_final: BTreeMap<(usize, usize), String> = BTreeMap::new();
        for event in &self.events {
            let value = Json::parse_with_limits(
                &event.data,
                self.limits.event,
                self.limits.json_depth,
                self.limits.json_nodes,
            )?;
            let ty = value.text_field("type")?;
            if event.name != "message" && event.name != ty {
                return Err("sse_event_name");
            };
            let current = value.usize_field("sequence_number")?;
            if current != seq {
                return Err("sequence_mismatch");
            };
            seq += 1;
            if ty != "response.created" && response_id.is_empty() {
                return Err("response_created_order");
            }
            if value.has("response_id")
                && (response_id.is_empty() || value.text_field("response_id")? != response_id)
            {
                return Err("event_response_identity");
            }
            if value.has("item_id") {
                let index = value.usize_field("output_index")?;
                let prior = added.get(&index).ok_or("orphan_item_id")?;
                if prior.text_field("id")? != value.text_field("item_id")?
                    || done.contains_key(&index)
                {
                    return Err("item_event_identity");
                }
                for field in ["call_id", "name", "namespace"] {
                    if value.has(field)
                        && (!prior.has(field) || value.field(field)? != prior.field(field)?)
                    {
                        return Err("event_call_identity_conflict");
                    }
                }
            }
            if terminal.is_some() {
                return Err("event_after_terminal");
            }
            match ty {
                "response.created" => {
                    if !response_id.is_empty() || seq != 1 {
                        return Err("response_created_order");
                    };
                    let response = value.field("response")?;
                    response_id = response.text_field("id")?.to_owned();
                    created_model = response.text_field("model")?.to_owned();
                    if !nonempty(&response_id)
                        || created_model != self.request.binding.returned_model
                        || response.text_field("status")? != "in_progress"
                        || !response.field("output")?.elements()?.is_empty()
                    {
                        return Err("created_binding");
                    }
                }
                "response.in_progress" => {
                    if progress {
                        return Err("duplicate_in_progress");
                    };
                    progress = true;
                    let r = value.field("response")?;
                    if r.text_field("id")? != response_id
                        || r.text_field("model")? != created_model
                        || r.text_field("status")? != "in_progress"
                        || !r.field("output")?.elements()?.is_empty()
                    {
                        return Err("progress_status");
                    }
                }
                "response.output_item.added" => {
                    let i = value.usize_field("output_index")?;
                    if i != added.len() || i >= self.limits.items {
                        return Err("item_order_or_limit");
                    };
                    let item = value.field("item")?.clone();
                    validate_item(&self.request, &item, false, &self.limits)?;
                    if added.insert(i, item.clone()).is_some()
                        || !ids.insert(item.text_field("id")?.to_owned())
                    {
                        return Err("duplicate_item");
                    }
                }
                "response.function_call_arguments.delta"
                | "response.function_call_arguments.done" => {
                    let i = value.usize_field("output_index")?;
                    let item_id = value.text_field("item_id")?;
                    let prior = added.get(&i).ok_or("orphan_argument_target")?;
                    if prior.text_field("id")? != item_id
                        || prior.text_field("type")? != "function_call"
                        || done.contains_key(&i)
                        || arg_final.contains_key(&i)
                    {
                        return Err("argument_target_or_lifecycle");
                    };
                    if ty.ends_with(".delta") {
                        let delta = value.text_field("delta")?;
                        let slot = arg_delta.entry(i).or_default();
                        if slot.len().saturating_add(delta.len()) > self.limits.arguments {
                            return Err("arguments_limit");
                        };
                        slot.push_str(delta);
                    } else {
                        let final_value = value.text_field("arguments")?.to_owned();
                        if final_value.len() > self.limits.arguments {
                            return Err("arguments_limit");
                        };
                        if arg_delta.get(&i).is_some_and(|d| d != &final_value) {
                            return Err("arguments_delta_conflict");
                        }
                        arg_final.insert(i, final_value);
                    }
                }
                "response.output_item.done" => {
                    let i = value.usize_field("output_index")?;
                    let item = value.field("item")?.clone();
                    let prior = added.get(&i).ok_or("orphan_item_done")?;
                    validate_item(&self.request, &item, true, &self.limits)?;
                    compatible_added(prior, &item)?;
                    if prior.text_field("id")? != item.text_field("id")?
                        || prior.text_field("type")? != item.text_field("type")?
                        || done.insert(i, item.clone()).is_some()
                    {
                        return Err("item_done_identity");
                    };
                    if item.text_field("type")? == "function_call" {
                        for k in ["id", "call_id", "name", "namespace"] {
                            if prior.has(k) != item.has(k)
                                || prior.has(k) && prior.field(k)? != item.field(k)?
                            {
                                return Err("added_metadata_conflict");
                            }
                        }
                        let args = item.text_field("arguments")?;
                        let initial = prior.text_field("arguments")?;
                        if (!initial.is_empty() && initial != args)
                            || arg_final.get(&i).is_some_and(|a| a != args)
                            || arg_delta.get(&i).is_some_and(|a| a != args)
                        {
                            return Err("arguments_final_conflict");
                        };
                        if !Json::parse_with_limits(
                            args,
                            self.limits.arguments,
                            self.limits.json_depth,
                            self.limits.json_nodes,
                        )?
                        .is_object()
                        {
                            return Err("arguments_object");
                        };
                        let name = item.text_field("name")?;
                        calls.push(FunctionCall {
                            item_id: item.text_field("id")?.to_owned(),
                            call_id: item.text_field("call_id")?.to_owned(),
                            name: name.to_owned(),
                            arguments: args.to_owned(),
                            output_index: i,
                        });
                        if calls.len() > self.limits.calls {
                            return Err("call_count_limit");
                        };
                    } else {
                        let key = if item.text_field("type")? == "reasoning" {
                            "summary"
                        } else {
                            "content"
                        };
                        let finals = item.field(key)?.elements()?;
                        let initial = prior.field(key)?.elements()?;
                        if !initial.is_empty() && initial != finals {
                            return Err("added_content_conflict");
                        };
                        let part_count = parts.keys().filter(|(index, _)| *index == i).count();
                        if part_count != 0 && part_count != finals.len() {
                            return Err("content_part_count");
                        }
                        for (j, part) in finals.iter().enumerate() {
                            if parts.keys().any(|(index, _)| *index == i)
                                && (part_done.get(&(i, j)) != Some(part))
                            {
                                return Err("content_part_final_conflict");
                            }
                        }
                    }
                }
                "response.metadata" => {
                    if value.text_field("response_id")? != response_id {
                        return Err("metadata_response_identity");
                    };
                    if !value.field("metadata")?.is_object() {
                        return Err("metadata_shape");
                    }
                }
                "response.content_part.added"
                | "response.content_part.done"
                | "response.output_text.delta"
                | "response.output_text.done"
                | "response.refusal.delta"
                | "response.refusal.done"
                | "response.reasoning_summary_part.added"
                | "response.reasoning_summary_part.done"
                | "response.reasoning_summary_text.delta"
                | "response.reasoning_summary_text.done" => {
                    let i = value.usize_field("output_index")?;
                    let prior = added.get(&i).ok_or("orphan_content_event")?;
                    let summary = ty.starts_with("response.reasoning_summary_");
                    let expected = if summary { "reasoning" } else { "message" };
                    if prior.text_field("type")? != expected
                        || prior.text_field("id")? != value.text_field("item_id")?
                    {
                        return Err("content_item_identity");
                    }
                    let j = value.usize_field(if summary {
                        "summary_index"
                    } else {
                        "content_index"
                    })?;
                    let key = (i, j);
                    let is_part = ty.contains("part.");
                    if is_part && ty.ends_with(".added") {
                        if j >= self.limits.items
                            || j != parts.keys().filter(|(idx, _)| *idx == i).count()
                        {
                            return Err("content_part_order");
                        }
                        let p = value.field("part")?.clone();
                        let kind = p.text_field("type")?;
                        if (summary && kind != "summary_text")
                            || !["summary_text", "output_text", "refusal"].contains(&kind)
                            || !p
                                .text_field(if kind == "refusal" { "refusal" } else { "text" })?
                                .is_empty()
                            || parts.insert(key, p).is_some()
                        {
                            return Err("content_part_added");
                        }
                    } else {
                        let old = parts.get(&key).ok_or("orphan_content_part")?;
                        let kind = old.text_field("type")?;
                        let field = if kind == "refusal" { "refusal" } else { "text" };
                        if is_part {
                            let p = value.field("part")?.clone();
                            if p.text_field("type")? != kind
                                || text_final
                                    .get(&key)
                                    .is_none_or(|t| p.text_field(field).ok() != Some(t.as_str()))
                                || part_done.insert(key, p).is_some()
                            {
                                return Err("content_part_final");
                            }
                        } else {
                            if text_final.contains_key(&key) {
                                return Err("text_after_done");
                            };
                            let wanted = if ty.starts_with("response.refusal.") {
                                "refusal"
                            } else if summary {
                                "summary_text"
                            } else {
                                "output_text"
                            };
                            if kind != wanted {
                                return Err("text_kind_conflict");
                            };
                            if ty.ends_with(".delta") {
                                let text = value.text_field("delta")?;
                                let slot = text_delta.entry(key).or_default();
                                if slot.len().saturating_add(text.len()) > self.limits.body {
                                    return Err("text_limit");
                                };
                                slot.push_str(text)
                            } else {
                                let text = value.text_field(field)?.to_owned();
                                if text_delta.get(&key).is_some_and(|d| d != &text) {
                                    return Err("text_delta_conflict");
                                };
                                text_final.insert(key, text);
                            }
                        }
                    }
                }
                "response.completed" => terminal = Some(value.field("response")?.clone()),
                "response.failed" | "response.incomplete" | "error" => {
                    return Err("response_unsuccessful");
                }
                _ => return Err("unknown_response_event"),
            }
        }
        let mut call_ids = HashSet::new();
        for item in added.values() {
            if item.text_field("type")? == "function_call"
                && (!call_ids.insert(item.text_field("call_id")?)
                    || call_ids.len() > self.limits.calls)
            {
                return Err("duplicate_call_id");
            }
        }
        if allow_prefix && terminal.is_none() {
            return Ok(None);
        }
        let terminal = terminal.ok_or("response_premature_eof")?;
        if terminal.text_field("id")? != response_id
            || terminal.text_field("model")? != created_model
            || terminal.text_field("model")? != self.request.binding.returned_model
            || terminal.text_field("status")? != "completed"
        {
            return Err("terminal_identity_or_status");
        };
        if terminal.has("error") && terminal.field("error")?.dump() != "null"
            || terminal.has("incomplete_details")
                && terminal.field("incomplete_details")?.dump() != "null"
        {
            return Err("terminal_error_metadata");
        };
        let mut items = Vec::new();
        let output = terminal.field("output")?.elements()?;
        if output.len() != added.len() || output.len() != done.len() {
            return Err("terminal_item_count");
        };
        for (i, item) in output.iter().enumerate() {
            if done.get(&i) != Some(item) {
                return Err("terminal_item_mismatch");
            };
            items.push(NativeItem::new(item.clone())?)
        }
        if calls.iter().any(|c| c.call_id.is_empty())
            || calls
                .iter()
                .map(|c| &c.call_id)
                .collect::<HashSet<_>>()
                .len()
                != calls.len()
        {
            return Err("duplicate_call_id");
        };
        // Completion events may arrive in a different order. Executable views
        // follow the ordered native output, as the C++ production builder does.
        calls.sort_by_key(|call| call.output_index);
        Ok(Some(ValidatedResponse {
            id: response_id,
            model: created_model,
            items,
            calls,
        }))
    }
}

#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
fn compatible_added(added: &Json, final_item: &Json) -> Result<(), &'static str> {
    for key in added.keys()? {
        if [
            "status",
            "arguments",
            "content",
            "summary",
            "encrypted_content",
        ]
        .contains(&key)
        {
            continue;
        };
        if !final_item.has(key) || added.field(key)? != final_item.field(key)? {
            return Err("added_metadata_conflict");
        }
    }
    if added.has("type")
        && added.text_field("type")? == "reasoning"
        && added.has("encrypted_content")
    {
        let initial = added.text_field("encrypted_content")?;
        if !initial.is_empty() && initial != final_item.text_field("encrypted_content")? {
            return Err("opaque_conflict");
        }
    }
    Ok(())
}

#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
fn nonempty(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256
}
#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
fn validate_item(
    request: &Request,
    item: &Json,
    final_item: bool,
    limits: &Limits,
) -> Result<(), &'static str> {
    if !item.is_object() || !nonempty(item.text_field("id")?) {
        return Err("item_object_or_id");
    };
    let ty = item.text_field("type")?;
    if !["function_call", "reasoning", "message"].contains(&ty) {
        return Err("unsupported_output_item");
    };
    if item.has("status") {
        let status = item.text_field("status")?;
        if final_item && status != "completed"
            || !final_item && status != "in_progress" && status != "completed"
        {
            return Err("item_status");
        }
    }
    match ty {
        "function_call" => {
            let call_id = item.text_field("call_id")?;
            let name = item.text_field("name")?;
            if !nonempty(call_id)
                || !nonempty(name)
                || item.text_field("namespace")? != "omh"
                || !request.tools.descriptors.iter().any(|d| d.name == name)
            {
                return Err("unadvertised_function");
            };
            let args = item.text_field("arguments")?;
            if args.len() > limits.arguments {
                return Err("arguments_limit");
            };
            if final_item
                && (!item.has("status")
                    || item.text_field("status")? != "completed"
                    || !Json::parse_with_limits(
                        args,
                        limits.arguments,
                        limits.json_depth,
                        limits.json_nodes,
                    )?
                    .is_object())
            {
                return Err("function_call_final");
            }
        }
        "reasoning" => {
            if item.has("encrypted_content") {
                item.text_field("encrypted_content")?;
            }
            for p in item.field("summary")?.elements()? {
                if p.text_field("type")? != "summary_text" {
                    return Err("reasoning_summary_type");
                };
                p.text_field("text")?;
            }
            if final_item
                && (!item.has("encrypted_content")
                    || item.text_field("encrypted_content")?.is_empty())
            {
                return Err("reasoning_opaque_missing");
            }
        }
        _ => {
            if item.text_field("role")? != "assistant"
                || !item.field("content")?.is_array()
                || final_item && (!item.has("status") || item.text_field("status")? != "completed")
            {
                return Err("message_shape");
            };
            for p in item.field("content")?.elements()? {
                if !["output_text", "refusal"].contains(&p.text_field("type")?) {
                    return Err("unsupported_content");
                };
                p.text_field(if p.text_field("type") == Ok("refusal") {
                    "refusal"
                } else {
                    "text"
                })?;
            }
        }
    }
    Ok(())
}

#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
pub struct ResponseStream {
    parser: Parser,
    reducer: Reducer,
    closed: bool,
    cancelled: Option<Box<dyn Fn() -> bool>>,
}
#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
impl ResponseStream {
    pub(crate) fn new(request: Request, limits: Limits) -> Self {
        Self::new_with_callbacks(request, limits, None, None)
    }
    pub(crate) fn new_with_callbacks(
        request: Request,
        limits: Limits,
        preview: Option<TextPreview>,
        cancelled: Option<Box<dyn Fn() -> bool>>,
    ) -> Self {
        let reducer = Reducer::new(request, limits, preview);
        Self {
            parser: Parser::new(limits),
            reducer,
            closed: false,
            cancelled,
        }
    }
    #[cfg(feature = "protocol-test-adapter")]
    #[allow(dead_code)]
    pub fn new_for_adapter(
        request: Request,
        limits: Limits,
        preview: Option<TextPreview>,
        cancelled: Option<Box<dyn Fn() -> bool>>,
    ) -> Self {
        Self::new_with_callbacks(request, limits, preview, cancelled)
    }
    #[cfg(feature = "protocol-test-adapter")]
    pub fn feed_for_adapter(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        self.feed(bytes)
    }
    #[cfg(feature = "protocol-test-adapter")]
    pub fn finish_for_adapter(self) -> Result<AdmittedResponse, &'static str> {
        self.finish()
    }
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        if self.closed {
            return Err("response_stream_closed");
        };
        if self.cancelled.as_ref().is_some_and(|cancelled| cancelled()) {
            self.closed = true;
            return Err("stream_cancelled");
        }
        if let Err(e) = self.parser.feed(bytes) {
            self.closed = true;
            return Err(e);
        }
        for event in self.parser.take() {
            if let Err(e) = self.reducer.event(event) {
                self.closed = true;
                return Err(e);
            }
            if self.cancelled.as_ref().is_some_and(|cancelled| cancelled()) {
                self.closed = true;
                return Err("stream_cancelled");
            }
        }
        Ok(())
    }
    pub(crate) fn finish(mut self) -> Result<AdmittedResponse, &'static str> {
        if self.closed {
            return Err("response_stream_closed");
        };
        if self.cancelled.as_ref().is_some_and(|cancelled| cancelled()) {
            return Err("stream_cancelled");
        }
        let events = self.parser.finish()?;
        for event in events {
            self.reducer.event(event)?;
            if self.cancelled.as_ref().is_some_and(|cancelled| cancelled()) {
                return Err("stream_cancelled");
            }
        }
        self.closed = true;
        self.reducer.finish()
    }
}

/// Closed offline adapter: it still uses the production framer and reducer.
#[cfg_attr(not(feature = "protocol-test-adapter"), allow(dead_code))]
pub(crate) fn decode_synthetic(
    request: Request,
    head_status: u16,
    mime: &str,
    chunks: &[&[u8]],
    limits: Limits,
) -> Result<AdmittedResponse, &'static str> {
    // Callers must first admit the HTTP head. Saved-response loading enters
    // through the same decoder with this fixed synthetic media type.
    let _ = (head_status, mime);
    let mut stream = ResponseStream::new(request, limits);
    for chunk in chunks {
        stream.feed(chunk)?
    }
    stream.finish()
}
