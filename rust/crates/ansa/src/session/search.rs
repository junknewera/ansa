//! Scoped search over validated journal records. Results never authorize work.
use super::{Result, codec_v1, journal_v1::inspect_records_read_only, need, state::State};
use crate::{
    auth::Cancel,
    protocol::json::Json,
    provider::{
        replay::{Effects, Execution},
        request::{Advertisement, Request},
    },
    tools::registry::Snapshot,
};
use std::path::Path;

pub(super) const HIT_LIMIT: usize = 32;

#[derive(Clone, Copy)]
pub(super) struct Scope<'a> {
    pub account: &'a str,
    pub provider: &'a str,
    pub cwd: &'a str,
}

#[derive(Debug, Default)]
pub(crate) struct Matches {
    pub hits: Vec<Hit>,
    pub limit_reached: bool,
    pub unavailable: bool,
}

#[derive(Debug)]
pub(crate) struct Hit {
    pub session: String,
    pub turn: String,
    pub sequence: u64,
    request: String,
    record_sha256: String,
    ordinal: usize,
    pub source: &'static str,
    pub snippet: String,
}

pub(super) fn normalize_query(query: &str) -> Result<String> {
    let query = query.trim();
    need(
        !query.is_empty() && query.len() <= 512 && query.chars().all(|c| !c.is_control()),
        "session_search_query",
    )?;
    Ok(query.to_lowercase())
}

fn visible(text: &str) -> impl Iterator<Item = char> {
    text.chars().map(|c| {
        if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            ' '
        } else {
            c
        }
    })
}

fn snippet(text: &str, query: &str) -> Option<String> {
    let text: String = visible(text).collect();
    let matched = text.to_lowercase().find(query)?;
    // Unicode lowercasing can expand a character (notably İ). Map folded bytes
    // back to a character boundary in the original before taking the excerpt.
    let mut offset = 0;
    let position = text
        .chars()
        .take_while(|character| {
            let before = offset;
            offset += character.to_lowercase().map(char::len_utf8).sum::<usize>();
            before < matched
        })
        .count();
    let start = position.saturating_sub(48);
    let mut chars = text.chars().skip(start);
    let mut result = String::new();
    if start > 0 {
        result.push_str("...");
    }
    result.extend(chars.by_ref().take(192));
    if chars.next().is_some() {
        result.push_str("...");
    }
    Some(result)
}

fn check_scope(request: &Request, scope: &Scope<'_>) -> Result<()> {
    need(
        request.binding.account == scope.account
            && request.binding.provider == scope.provider
            && request.final_format().is_none(),
        "session_search_scope",
    )?;
    let mut state = &request.epoch.exact_state;
    for _ in 0..16 {
        need(
            !state.has("orchestration_parent") && !state.has("local_console_parent"),
            "session_search_scope",
        )?;
        if state.has("base_state") {
            state = state.field("base_state")?;
        } else if state.has("base") {
            state = state.field("base")?;
        } else {
            return need(
                state.field("settings")?.text_field("cwd")? == scope.cwd,
                "session_search_scope",
            );
        }
    }
    Err("session_epoch_depth")
}

fn searchable_outcome(tool: &str, event: &Json) -> Result<String> {
    let saved = codec_v1::SavedResult::load(event.field("result")?)?;
    // Journal results are often file bodies, command output or MCP data. Search
    // the execution metadata while keeping those payloads outside this index.
    let execution = match saved.result.execution() {
        Execution::Succeeded => "succeeded",
        Execution::Failed => "failed",
        Execution::Cancelled => "cancelled",
        Execution::Denied => "denied",
    };
    let effects = match saved.result.effects() {
        Effects::NotStarted => "not_started",
        Effects::KnownResult => "known_result",
        Effects::Unknown => "effect_unknown",
    };
    Ok(format!("{tool} {execution} {effects}"))
}

#[allow(clippy::too_many_arguments)]
fn scan(
    path: &Path,
    session: &str,
    scope: Scope<'_>,
    compiled: &Snapshot,
    budget: &mut usize,
    stop: &Cancel,
    include: impl Fn(&str) -> bool,
    mut visit: impl FnMut(u64, usize, &str, &super::state::Receipt, &'static str, &str),
) -> Result<()> {
    let mut canonical = None::<State>;
    inspect_records_read_only(
        path,
        budget,
        || stop.check(),
        |sequence, hash, event| {
            let kind = event.text_field("kind")?;
            if canonical.is_none() {
                need(
                    kind == "init" && event.text_field("session")? == session,
                    "session_missing_init",
                )?;
                let configuration = Request::load(event.field("configuration")?)?;
                check_scope(&configuration, &scope)?;
                let plan = compiled
                    .verified_plan_tools(&configuration.tools)
                    .unwrap_or(Advertisement {
                        revision: String::new(),
                        descriptors: vec![],
                    });
                canonical = Some(State::new(session.into(), configuration, plan));
            }
            let state = canonical.as_mut().ok_or("session_missing_init")?;
            let tool = state
                .turn
                .as_ref()
                .and_then(|turn| turn.intent.as_ref())
                .map(|intent| intent.name.clone());
            let unanswered = if kind == "steered" {
                state
                    .pending_followups()
                    .filter(|m| m.state == "queued")
                    .map(|m| m.text.clone())
                    .collect::<Vec<_>>()
            } else {
                vec![]
            };
            state.apply(event)?;
            let Some(turn) = &state.turn else {
                return Ok(());
            };

            if turn.receipt.internal || !include(&turn.receipt.request.0) {
                return Ok(());
            }
            let receipt = &turn.receipt;
            let mut ordinal = 0;
            let mut add = |source, text: &str| {
                visit(sequence, ordinal, hash, receipt, source, text);
                ordinal += 1;
            };
            match kind {
                "accepted" => add("User", &turn.receipt.text),
                "side_reply" => {
                    let id = event.text_field("command")?;
                    let message = state
                        .followups
                        .iter()
                        .find(|m| m.command.0 == id)
                        .ok_or("session_side_reply")?;
                    add("User", &message.text);
                    let response = codec_v1::load_response(event.field("response")?)?;
                    add("Assistant", &codec_v1::response_text(&response)?);
                }
                "steered" => {
                    for text in unanswered {
                        add("User", &text);
                    }
                }
                "done" => {
                    let response = turn.response.as_ref().ok_or("session_done_stage")?;
                    add("Assistant", &codec_v1::response_text(response)?);
                }
                "outcome" => {
                    if let Some(tool) = tool {
                        add("Tool outcome", &searchable_outcome(&tool, event)?);
                    }
                }
                _ => {}
            }
            Ok(())
        },
    )?;
    need(canonical.is_some(), "session_missing_init")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn read(
    path: &Path,
    session: &str,
    scope: Scope<'_>,
    compiled: &Snapshot,
    query: &str,
    budget: &mut usize,
    max: usize,
    stop: &Cancel,
) -> Result<Vec<Hit>> {
    let mut hits = Vec::new();
    scan(
        path,
        session,
        scope,
        compiled,
        budget,
        stop,
        |_| true,
        |sequence, ordinal, hash, receipt, source, text| {
            if hits.len() < max
                && let Some(snippet) = snippet(text, query)
            {
                hits.push(Hit {
                    session: session.into(),
                    turn: receipt.turn.0.clone(),
                    request: receipt.request.0.clone(),
                    sequence,
                    record_sha256: hash.into(),
                    ordinal,
                    source,
                    snippet,
                });
            }
        },
    )?;
    Ok(hits)
}

pub(super) fn open(
    path: &Path,
    hit: &Hit,
    scope: Scope<'_>,
    compiled: &Snapshot,
    budget: &mut usize,
    stop: &Cancel,
) -> Result<Vec<(String, String)>> {
    let mut before = std::collections::VecDeque::new();
    let mut after = Vec::new();
    let mut matched = false;
    scan(
        path,
        &hit.session,
        scope,
        compiled,
        budget,
        stop,
        |request| request == hit.request,
        |sequence, ordinal, hash, receipt, source, text| {
            if receipt.turn.0 != hit.turn {
                return;
            }
            let target = (sequence, ordinal) == (hit.sequence, hit.ordinal);
            if target && source == hit.source && hash == hit.record_sha256 {
                matched = true;
            }
            let mut excerpt: String = visible(text).take(768).collect();
            if target && !excerpt.contains(&hit.snippet) && text.chars().nth(768).is_some() {
                excerpt = hit.snippet.clone();
            } else if text.chars().nth(768).is_some() {
                excerpt.push_str("...");
            }
            let item = (format!("{source} · {sequence}"), excerpt);
            if (sequence, ordinal) < (hit.sequence, hit.ordinal) {
                if before.len() == 8 {
                    before.pop_front();
                }
                before.push_back(item);
            } else if after.len() < 9 {
                after.push(item);
            }
        },
    )?;
    need(matched, "session_search_source_changed")?;
    Ok(before.into_iter().chain(after).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        frontend::app,
        provider::{contracts::AgentMode, request::Epoch},
        session::{
            catalog::{Catalog, Selected},
            store::Store,
        },
    };
    use std::{fs, io::Write, path::PathBuf};

    struct Root(PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_session(selected: &Selected, cwd: &Path, account: &str, tools: &Snapshot) {
        let string = |value| Json::string(value).unwrap();
        let binding = crate::provider::request::Binding {
            account: account.into(),
            ..app::binding()
        };
        let epoch = Epoch {
            id: "scope_fixture".into(),
            instructions: "fixture instructions".into(),
            exact_state: Json::object(vec![(
                "settings",
                Json::object(vec![("cwd", string(cwd.to_str().unwrap()))]).unwrap(),
            )])
            .unwrap(),
        };
        let config = Request::new(binding, epoch, tools.advertisement().clone(), vec![]).unwrap();
        let state = State::new(
            selected.id.as_str().into(),
            config,
            tools.advertisement().clone(),
        );
        let prompt = format!("{} Искомое слово", "a".repeat(240));
        let next = state.request_for(&prompt, AgentMode::Basic).unwrap();
        let mut store = Store::open(&selected.directory, state, None).unwrap();
        store
            .commit(
                &Json::object(vec![
                    ("kind", string("accepted")),
                    ("command", string("command")),
                    ("turn", string("turn")),
                    ("request", string("request")),
                    ("request_number", Json::parse("1").unwrap()),
                    ("text", string(&prompt)),
                    ("operation_run", string("run")),
                    ("next", next.save().unwrap()),
                    ("compact", Json::parse("false").unwrap()),
                ])
                .unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn scoped_search_discards_hits_from_semantically_invalid_or_torn_journals() {
        let root =
            Root(std::env::temp_dir().join(format!("ansa-search-scope-{}", std::process::id())));
        fs::create_dir(&root.0).unwrap();
        let directory = root.0.join("sessions");
        let cwd = root.0.join("workspace");
        let tools = app::pure_tools().unwrap();
        let catalog = Catalog::open(&directory, &cwd, "offline").unwrap();
        for (id, workspace, account) in [
            ("correct", cwd.clone(), "account"),
            ("other_account", cwd.clone(), "another"),
            (
                "spoofed_workspace",
                root.0.join("another-workspace"),
                "account",
            ),
            ("torn", cwd.clone(), "account"),
            ("invalid", cwd.clone(), "account"),
        ] {
            let selected = catalog.create_named(id).unwrap();
            make_session(&selected, &workspace, account, &tools);
            if id == "torn" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(selected.directory.join("part-00000000000000000000.log"))
                    .unwrap()
                    .write_all(b"torn")
                    .unwrap();
            } else if id == "invalid" {
                super::super::journal_v1::Journal::open(&selected.directory, |_| Ok(()), None)
                    .unwrap()
                    .append(&Json::parse(r#"{"kind":"done","turn":"turn"}"#).unwrap())
                    .unwrap();
            }
        }
        let selected = catalog.create_kind("agent", Some("correct")).unwrap();
        make_session(&selected, &cwd, "account", &tools);
        let catalog = Catalog::open_read_only(&directory, &cwd, "offline").unwrap();
        let stop = Cancel::default();
        let hits = catalog
            .search("ИСКОМОЕ", "account", "openai-siwc", &tools, &stop)
            .unwrap();
        assert!(hits.unavailable);
        assert!(!hits.limit_reached);
        assert_eq!(hits.hits.len(), 1);
        let empty = catalog
            .search("absent", "account", "openai-siwc", &tools, &stop)
            .unwrap();
        assert!(empty.hits.is_empty() && empty.unavailable);
        let limited = catalog
            .search_bounded("ИСКОМОЕ", "account", "openai-siwc", &tools, &stop, 1)
            .unwrap();
        assert!(limited.hits.is_empty() && limited.limit_reached);
        let hit = &hits.hits[0];
        assert_eq!(
            (&*hit.session, &*hit.turn, hit.sequence, hit.source),
            ("correct", "turn", 2, "User")
        );
        assert!(hit.snippet.contains("Искомое"));
        assert!(hit.snippet.chars().count() <= 198);
        let source = catalog
            .search_source(hit, "account", "openai-siwc", &tools, &stop)
            .unwrap();
        assert_eq!(source.len(), 1);
        assert!(source[0].1.contains("Искомое слово"));
        let part = directory.join("correct/part-00000000000000000000.log");
        let before = fs::read(&part).unwrap();
        catalog
            .search_source(hit, "account", "openai-siwc", &tools, &stop)
            .unwrap();
        assert_eq!(fs::read(&part).unwrap(), before);
        fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .unwrap()
            .write_all(b"torn")
            .unwrap();
        assert!(
            catalog
                .search_source(hit, "account", "openai-siwc", &tools, &stop)
                .is_err()
        );
        stop.cancel();
        assert!(
            catalog
                .search("ИСКОМОЕ", "account", "openai-siwc", &tools, &stop)
                .is_err()
        );
    }
}
