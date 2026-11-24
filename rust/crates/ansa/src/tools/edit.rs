//! Explicit, bounded UTF-8 edits with exact before/after values for the UI.
use super::{
    files::{Host, b, path_argument, schema},
    registry::{Definition, Outcome, Policy, Registry},
};
use crate::{
    auth::Cancel,
    config::{Result, path_text, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::{
        replay::{Effects, Execution},
        request::Descriptor,
    },
};
use std::{path::Path, sync::Arc};

const ARG_BYTES: usize = 16 * 1024;
const OUTPUT_BYTES: usize = 120 * 1024;

fn rejection(code: &str) -> Outcome {
    // These preflight checks conclusively rejected the edit. Return the receipt
    // to the model so it can report the blocker or choose another allowed task.
    let effects = match code {
        "terminal_workspace_quarantined" | "terminal_workspace_busy" | "task_path_scope" => {
            Effects::KnownResult
        }
        _ => Effects::NotStarted,
    };
    let mut fields = vec![
        ("error", s(code).expect("error code")),
        ("applied", b(false)),
    ];
    if code == "terminal_workspace_quarantined" {
        fields.push(("message", s("Inspect the terminal lease with ansa --workspace-lease from this workspace. The command identifies its owning session.").expect("lease hint")));
    }
    Outcome::from_bytes(
        Execution::Failed,
        effects,
        Json::object(fields)
            .expect("edit rejection")
            .dump()
            .into_bytes(),
    )
}
fn unknown() -> Outcome {
    Outcome::from_bytes(
        Execution::Failed,
        Effects::Unknown,
        br#"{"error":"file_edit_effect_unknown"}"#.to_vec(),
    )
}

impl Host {
    #[cfg(test)]
    fn edit(&self, args: &Json, stop: &Cancel) -> Outcome {
        self.edit_owned(args, stop, &sha256(args.dump().as_bytes()))
    }

    fn edit_owned(&self, args: &Json, stop: &Cancel, operation: &str) -> Outcome {
        if stop.cancelled() {
            return Outcome::not_started();
        }
        self.edit_checked(args, stop, operation)
            .unwrap_or_else(rejection)
    }

    fn edit_checked(&self, args: &Json, stop: &Cancel, operation: &str) -> Result<Outcome> {
        let text = args.text_field("path")?;
        let path = Path::new(text);
        if !path_argument(args, "path")? {
            return Err("protected_file");
        }
        let original = if path.is_absolute() {
            path.to_owned()
        } else {
            self.settings.cwd.join(path)
        };
        let path = ansa_linux::fs::normalize(&original);
        self.check_task_path(&path, true)?;
        if self.protected_path(&path) {
            return Err("protected_file");
        }
        // Resolve for auth/session scope checks, but open the original path
        // without following any component, including its final filename.
        let parent = self.resolve_path(path.parent().ok_or("file_path")?, true)?;
        let resolved = parent.join(path.file_name().ok_or("file_path")?);
        self.check_task_path(&resolved, true)?;
        if self.protected_path(&resolved) {
            return Err("protected_file");
        }
        if let Some(instructions) = self.instructions(&parent, args)? {
            return Ok(Outcome::from_bytes(
                Execution::Failed,
                // The instruction handshake completed with a known response.
                // NotStarted would pause the turn before the model can receive
                // the returned token and acknowledge the required instructions.
                Effects::KnownResult,
                instructions.output().to_vec(),
            ));
        }
        let old = args.text_field("old_text")?;
        let new = args.text_field("new_text")?;
        if old.len() > ARG_BYTES
            || new.len() > ARG_BYTES
            || old.contains('\0')
            || new.contains('\0')
        {
            return Err("file_edit_limit");
        }
        let mut workspace = self.workspace_writer(path.parent().ok_or("file_path")?)?;
        let snapshot = ansa_linux::file_edit::Snapshot::open_in(&path, workspace.directory())?;
        let existed = snapshot.before().is_some();
        let before = std::str::from_utf8(snapshot.before().unwrap_or_default())
            .map_err(|_| "file_not_utf8")?;
        if before.contains('\0') {
            return Err("binary_file");
        }
        if stop.cancelled() {
            return Ok(Outcome::not_started());
        }
        let after = if old.is_empty() {
            if existed {
                return Err("file_already_exists");
            }
            new.to_owned()
        } else {
            if !existed {
                return Err("file_missing");
            }
            let start = before.find(old).ok_or("file_preimage_missing")?;
            let next = start + old.chars().next().expect("nonempty preimage").len_utf8();
            if before[next..].contains(old) {
                return Err("file_preimage_ambiguous");
            }
            if old == new {
                return Err("file_edit_no_change");
            }
            let mut after =
                String::with_capacity(before.len().saturating_sub(old.len()) + new.len());
            after.push_str(&before[..start]);
            after.push_str(new);
            after.push_str(&before[start + old.len()..]);
            after
        };
        if after.len() > ansa_linux::file_edit::MAX_BYTES {
            return Err("file_edit_limit");
        }
        let output = Json::object(vec![
            ("path", s(path_text(&path)?)?),
            ("before", s(before)?),
            ("after", s(&after)?),
            ("before_hash", s(&sha256(before.as_bytes()))?),
            ("after_hash", s(&sha256(after.as_bytes()))?),
            ("applied", b(true)),
        ])?
        .dump();
        // A successful edit must always have a complete, bounded result. This
        // check precedes creation of the private staging file and publication.
        if output.len() > OUTPUT_BYTES {
            return Err("file_edit_output_limit");
        }
        workspace.begin(operation)?;
        let published = snapshot.publish(after.as_bytes(), || stop.cancelled());
        if published != Err(ansa_linux::file_edit::Failure::Unknown) {
            let _ = workspace.finish();
        }
        match published {
            Ok(()) => Ok(Outcome::succeeded(output)),
            Err(ansa_linux::file_edit::Failure::Cancelled) => Ok(Outcome::not_started()),
            Err(ansa_linux::file_edit::Failure::Rejected(code)) => Err(code),
            Err(ansa_linux::file_edit::Failure::Unknown) => Ok(unknown()),
        }
    }
}

pub(crate) fn register(registry: &mut Registry, host: Arc<Host>) -> Result<()> {
    registry.register_with_policy(Definition {
        descriptor: Descriptor {
            name: "edit_file".into(),
            description: "Replace exactly one old_text occurrence in a regular UTF-8 file. To create a new file, use empty old_text; the path must not exist. Read the file first. Files are limited to 64 KiB, old_text/new_text to 16 KiB each; a complete before/after result must fit 120 KiB. Dotenv, credentials, links and ansa state are excluded. instruction_token identifies applicable AGENTS.md instructions.".into(),
            revision: "ansa_edit_v1".into(),
            schema: schema(&[("path", "string"), ("old_text", "string"), ("new_text", "string")])?,
        },
        validator: Arc::new(|args| edit_argument_hint(args).is_none()),
        // The legacy no-journal executor cannot publish a file.
        executor: Arc::new(|_| rejection("durable_checkpoint_required")),
    }, Policy {
        argument_hint: Some(Arc::new(edit_argument_hint)),
        requires_journal: true,
        read_only: false,
        invoke: Some(Arc::new(move |invocation, stop| match invocation.save() {
            Ok(exact) => host.edit_owned(&invocation.arguments, stop, &sha256(exact.dump().as_bytes())),
            Err(code) => rejection(code),
        })),
        // An interrupted publication has no safely replayable external effect.
        // The canonical pending intent remains unknown until reconciliation.
        recover: Some(Arc::new(|_| Ok(None))),
    })
}

fn edit_argument_hint(args: &Json) -> Option<&'static str> {
    if !path_argument(args, "path").unwrap_or(false) {
        return Some(
            "Use an allowed workspace path (1..4096 bytes, no NUL) and an instruction_token of at most 64 bytes.",
        );
    }
    for (field, hint) in [
        (
            "old_text",
            "old_text must contain at most 16384 bytes and no NUL. Match a smaller section.",
        ),
        (
            "new_text",
            "new_text must contain at most 16384 bytes and no NUL. Split the change into smaller edits.",
        ),
    ] {
        if !args
            .text_field(field)
            .is_ok_and(|text| text.len() <= ARG_BYTES && !text.contains('\0'))
        {
            return Some(hint);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{agent_epoch, load_agent_settings},
        provider::{
            native,
            request::{Binding, Request},
        },
        tools::{
            admission::{OperationIds, PreparedBatch},
            command::ProcessRunner,
            files::local_tools,
            invocation::Invocation,
            registry::Snapshot,
        },
    };
    use std::{
        os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink},
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "ansa-edit-tool-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            std::fs::create_dir_all(path.join("home/workspace")).unwrap();
            Self(path)
        }
        fn cwd(&self) -> PathBuf {
            self.0.join("home/workspace")
        }
        fn path(&self) -> PathBuf {
            self.cwd().join("file")
        }
        fn host(&self) -> Host {
            let settings = load_agent_settings(&self.cwd(), &self.0.join("home")).unwrap();
            Host {
                epoch: agent_epoch(&settings).unwrap(),
                processes: ProcessRunner::new(
                    &self.0.join("session/operations"),
                    &std::env::current_exe().unwrap(),
                )
                .unwrap(),
                auth: settings.directory.join("auth"),
                session: self.0.join("session"),
                editor: None,
                shared_writer: None,
                scope: None,
                settings,
            }
        }
        fn args(&self, host: &Host, path: &Path, old: &str, new: &str) -> Json {
            Json::object(vec![
                ("path", s(path.to_str().unwrap()).unwrap()),
                ("old_text", s(old).unwrap()),
                ("new_text", s(new).unwrap()),
                (
                    "instruction_token",
                    s(&sha256(
                        host.epoch
                            .exact_state
                            .field("instructions")
                            .unwrap()
                            .dump()
                            .as_bytes(),
                    ))
                    .unwrap(),
                ),
            ])
            .unwrap()
        }
        fn snapshot(&self, host: &Host) -> Arc<Snapshot> {
            local_tools(
                host.settings.clone(),
                host.epoch.clone(),
                &self.0.join("session"),
                &host.auth,
                &std::env::current_exe().unwrap(),
            )
            .unwrap()
        }
    }

    #[test]
    fn workspace_quarantine_returns_a_known_refusal_without_writing() {
        let root = Root::new();
        let host = root.host();
        std::fs::write(root.path(), "before").unwrap();
        let mut writer = host.workspace_writer(&root.cwd()).unwrap();
        writer.begin(&"a".repeat(64)).unwrap();
        drop(writer);
        let result = host.edit(
            &root.args(&host, &root.path(), "before", "after"),
            &Cancel::default(),
        );
        assert_eq!(
            (result.execution(), result.effects()),
            (Execution::Failed, Effects::KnownResult)
        );
        assert_eq!(
            output(&result).text_field("error"),
            Ok("terminal_workspace_quarantined")
        );
        assert_eq!(*output(&result).field("applied").unwrap(), b(false));
        assert_eq!(std::fs::read_to_string(root.path()).unwrap(), "before");
    }

    #[test]
    fn task_scope_blocks_writes_outside_approval_and_filters_executors() {
        let root = Root::new();
        let mut host = root.host();
        host.scope =
            Some(crate::tools::files::TaskScope::new(&root.cwd(), &["file".into()]).unwrap());
        std::fs::write(root.path(), "before").unwrap();
        let other = root.cwd().join("file-other");
        std::fs::write(&other, "before").unwrap();
        let outside = root.0.join("outside");
        std::fs::write(&outside, "before").unwrap();
        assert!(host.path_os(&outside).is_err());
        host.scope = Some(
            crate::tools::files::TaskScope::new(&root.cwd(), &["file".into()])
                .unwrap()
                .restrict_reads(&["file".into()])
                .unwrap(),
        );
        assert_eq!(host.path_os(&other), Err("task_path_scope"));
        assert_eq!(host.path_os(&root.path()), Ok(root.path()));
        assert_eq!(host.task_directory_path("."), Ok(root.cwd()));
        let stop = Cancel::default();
        for denied in [&other, &outside] {
            let result = host.edit(&root.args(&host, denied, "before", "after"), &stop);
            assert_eq!(result.execution(), Execution::Failed);
            assert_eq!(result.effects(), Effects::KnownResult);
            assert_eq!(output(&result).text_field("error"), Ok("task_path_scope"));
            assert_eq!(*output(&result).field("applied").unwrap(), b(false));
            assert_eq!(std::fs::read_to_string(denied).unwrap(), "before");
        }
        let result = host.edit(&root.args(&host, &root.path(), "before", "after"), &stop);
        assert_eq!(result.execution(), Execution::Succeeded);
        assert_eq!(std::fs::read_to_string(root.path()).unwrap(), "after");
        let snapshot = crate::tools::files::local_task_tools(
            host.settings.clone(),
            host.epoch.clone(),
            &host.session,
            &host.auth,
            &std::env::current_exe().unwrap(),
            crate::tools::files::TaskScope::new(&root.cwd(), &[]).unwrap(),
        )
        .unwrap();
        assert_eq!(
            snapshot
                .advertisement()
                .descriptors
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["list_directory", "read_file", "search_files"]
        );
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn output(value: &Outcome) -> Json {
        Json::parse(std::str::from_utf8(value.output()).unwrap()).unwrap()
    }
    fn rejected(value: Outcome, error: &str) {
        assert_eq!(value.execution(), Execution::Failed);
        assert_eq!(value.effects(), Effects::NotStarted);
        let output = output(&value);
        assert_eq!(output.text_field("error").unwrap(), error);
        assert_eq!(*output.field("applied").unwrap(), b(false));
    }
    fn response(
        host: &Host,
        snapshot: &Snapshot,
        arguments: &[Json],
    ) -> crate::provider::responses::AdmittedResponse {
        let request = Request::new(
            Binding {
                provider: "openai-siwc".into(),
                account: "offline-fixture".into(),
                requested_model: "gpt-6.1-sol".into(),
                returned_model: "gpt-6.1-sol".into(),
            },
            host.epoch.clone(),
            snapshot.advertisement().clone(),
            Vec::new(),
        )
        .unwrap();
        let items = arguments
            .iter()
            .enumerate()
            .map(|(index, args)| {
                Json::object(vec![
                    ("type", s("function_call").unwrap()),
                    ("id", s(&format!("item-{index}")).unwrap()),
                    ("call_id", s(&format!("call-{index}")).unwrap()),
                    ("name", s("edit_file").unwrap()),
                    ("namespace", s("omh").unwrap()),
                    ("arguments", s(&args.dump()).unwrap()),
                    ("status", s("completed").unwrap()),
                ])
                .unwrap()
            })
            .collect();
        native::load(
            &Json::object(vec![
                ("request", request.save().unwrap()),
                ("id", s("response").unwrap()),
                ("model", s("gpt-6.1-sol").unwrap()),
                ("items", Json::array(items).unwrap()),
            ])
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn successful_edit_returns_exact_full_text_hashes_and_preserves_permissions() {
        let root = Root::new();
        let host = root.host();
        let before = "first\nhello Ж😀\nlast\n";
        let after = "first\nПривет 界\nlast\n";
        std::fs::write(root.path(), before).unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o751)).unwrap();
        let value = host.edit(
            &root.args(&host, &root.path(), "hello Ж😀", "Привет 界"),
            &Cancel::default(),
        );
        assert_eq!(
            (value.execution(), value.effects()),
            (Execution::Succeeded, Effects::KnownResult)
        );
        let output = output(&value);
        assert_eq!(
            output.text_field("path").unwrap(),
            root.path().to_str().unwrap()
        );
        assert_eq!(output.text_field("before").unwrap(), before);
        assert_eq!(output.text_field("after").unwrap(), after);
        assert_eq!(
            output.text_field("before_hash").unwrap(),
            sha256(before.as_bytes())
        );
        assert_eq!(
            output.text_field("after_hash").unwrap(),
            sha256(after.as_bytes())
        );
        assert_eq!(*output.field("applied").unwrap(), b(true));
        assert_eq!(std::fs::read_to_string(root.path()).unwrap(), after);
        assert_eq!(
            std::fs::metadata(root.path()).unwrap().mode() & 0o777,
            0o751
        );
    }

    #[test]
    fn absent_overlapping_or_ambiguous_preimages_and_noops_do_not_write() {
        let root = Root::new();
        let host = root.host();
        for (before, old, new, error) in [
            ("aaa", "aa", "x", "file_preimage_ambiguous"),
            ("one one", "one", "two", "file_preimage_ambiguous"),
            ("one", "missing", "two", "file_preimage_missing"),
            ("one", "one", "one", "file_edit_no_change"),
            ("", "", "new", "file_already_exists"),
        ] {
            std::fs::write(root.path(), before).unwrap();
            rejected(
                host.edit(
                    &root.args(&host, &root.path(), old, new),
                    &Cancel::default(),
                ),
                error,
            );
            assert_eq!(std::fs::read_to_string(root.path()).unwrap(), before);
        }
    }

    #[test]
    fn creation_and_cancel_are_explicit_and_creation_never_overwrites() {
        let root = Root::new();
        let host = root.host();
        rejected(
            host.edit(
                &root.args(&host, &root.path(), "old", "new"),
                &Cancel::default(),
            ),
            "file_missing",
        );
        let args = root.args(&host, &root.path(), "", "new file\n");
        let stop = Cancel::default();
        stop.cancel();
        let cancelled = host.edit(&args, &stop);
        assert_eq!(
            (cancelled.execution(), cancelled.effects()),
            (Execution::Cancelled, Effects::NotStarted)
        );
        assert!(!root.path().exists());
        let applied = host.edit(&args, &Cancel::default());
        assert_eq!(applied.effects(), Effects::KnownResult);
        assert_eq!(output(&applied).text_field("before").unwrap(), "");
        rejected(host.edit(&args, &Cancel::default()), "file_already_exists");
        assert_eq!(std::fs::read_to_string(root.path()).unwrap(), "new file\n");
    }

    #[test]
    fn protected_roots_and_link_aliases_are_rejected_before_read_or_write() {
        let root = Root::new();
        let host = root.host();
        for path in [
            root.cwd().join(".env"),
            root.cwd().join(".env.local"),
            root.cwd().join(".codex/file"),
            root.cwd().join(".claude/file"),
            root.cwd().join(".gemini/file"),
        ] {
            rejected(
                host.edit(&root.args(&host, &path, "", "new"), &Cancel::default()),
                "protected_file",
            );
        }
        for directory in [host.auth.clone(), host.settings.directory.join("sessions")] {
            assert!(!directory.exists());
            rejected(
                host.edit(&root.args(&host, &directory, "", "new"), &Cancel::default()),
                "protected_file",
            );
            assert!(
                !directory.exists(),
                "a protected root became a regular file"
            );
            std::fs::create_dir_all(&directory).unwrap();
            rejected(
                host.edit(
                    &root.args(&host, &directory.join("new"), "", "new"),
                    &Cancel::default(),
                ),
                "protected_file",
            );
        }
        std::fs::write(root.path(), "safe").unwrap();
        symlink(root.path(), root.cwd().join("alias")).unwrap();
        assert_eq!(
            host.edit(
                &root.args(&host, &root.cwd().join("alias"), "safe", "new"),
                &Cancel::default()
            )
            .effects(),
            Effects::NotStarted
        );
        std::fs::hard_link(root.path(), root.cwd().join("hardlink")).unwrap();
        assert_eq!(
            host.edit(
                &root.args(&host, &root.path(), "safe", "new"),
                &Cancel::default()
            )
            .effects(),
            Effects::NotStarted
        );
        std::fs::remove_file(root.cwd().join("hardlink")).unwrap();
        symlink(root.cwd(), root.cwd().join("linked-directory")).unwrap();
        assert_eq!(
            host.edit(
                &root.args(
                    &host,
                    &root.cwd().join("linked-directory/file"),
                    "safe",
                    "new"
                ),
                &Cancel::default()
            )
            .effects(),
            Effects::NotStarted
        );
        assert_eq!(std::fs::read_to_string(root.path()).unwrap(), "safe");
    }

    #[test]
    fn file_argument_output_and_utf8_limits_precede_publication() {
        let root = Root::new();
        let host = root.host();
        let large = format!("x{}", "a".repeat(ansa_linux::file_edit::MAX_BYTES - 1));
        std::fs::write(root.path(), &large).unwrap();
        rejected(
            host.edit(
                &root.args(&host, &root.path(), "x", "y"),
                &Cancel::default(),
            ),
            "file_edit_output_limit",
        );
        rejected(
            host.edit(
                &root.args(&host, &root.path(), "x", "yy"),
                &Cancel::default(),
            ),
            "file_edit_limit",
        );
        assert_eq!(std::fs::read_to_string(root.path()).unwrap(), large);
        rejected(
            host.edit(
                &root.args(&host, &root.path(), "x", &"z".repeat(ARG_BYTES + 1)),
                &Cancel::default(),
            ),
            "file_edit_limit",
        );
        std::fs::write(root.path(), [0xff]).unwrap();
        rejected(
            host.edit(
                &root.args(&host, &root.path(), "x", "y"),
                &Cancel::default(),
            ),
            "file_not_utf8",
        );
        std::fs::write(root.path(), b"nul\0file").unwrap();
        rejected(
            host.edit(
                &root.args(&host, &root.path(), "nul", "safe"),
                &Cancel::default(),
            ),
            "binary_file",
        );
        assert_eq!(std::fs::read(root.path()).unwrap(), b"nul\0file");
    }

    #[test]
    fn actual_nondefault_session_root_is_excluded_from_edit_read_and_search() {
        let root = Root::new();
        let host = root.host();
        let session = root.cwd().join("fixture/edit-session");
        let snapshot = local_tools(
            host.settings.clone(),
            host.epoch.clone(),
            &session,
            &host.auth,
            &std::env::current_exe().unwrap(),
        )
        .unwrap();
        let journal = session.join("part-000000.log");
        let evidence = session.join("operations/outcome");
        std::fs::write(&journal, "PRIVATE_JOURNAL_CANARY").unwrap();
        std::fs::write(&evidence, "PRIVATE_EVIDENCE_CANARY").unwrap();
        let invoke = snapshot
            .policy("edit_file")
            .unwrap()
            .invoke
            .as_ref()
            .unwrap();
        for (path, old) in [
            (&session, ""),
            (&journal, "PRIVATE"),
            (&evidence, "PRIVATE"),
        ] {
            let invocation = Invocation {
                operation: "fixture:op:1".into(),
                call: "call".into(),
                name: "edit_file".into(),
                revision: "ansa_edit_v1".into(),
                arguments: root.args(&host, path, old, "overwrite"),
            };
            rejected(invoke(&invocation, &Cancel::default()), "protected_file");
        }
        let token = root
            .args(&host, &journal, "", "")
            .field("instruction_token")
            .unwrap()
            .clone();
        let read = Json::object(vec![
            ("path", s(journal.to_str().unwrap()).unwrap()),
            ("offset", Json::parse("0").unwrap()),
            ("max_bytes", Json::parse("8192").unwrap()),
            ("instruction_token", token.clone()),
        ])
        .unwrap();
        let value = (snapshot.definition("read_file").unwrap().executor)(&read);
        assert_eq!(
            output(&value).text_field("error").unwrap(),
            "protected_file"
        );
        std::fs::write(root.cwd().join("public"), "PUBLIC_CANARY").unwrap();
        let search = Json::object(vec![
            ("path", s(root.cwd().to_str().unwrap()).unwrap()),
            ("query", s("PRIVATE_").unwrap()),
            ("mode", s("content").unwrap()),
            ("max_results", Json::parse("10").unwrap()),
            ("instruction_token", token),
        ])
        .unwrap();
        let value = (snapshot.definition("search_files").unwrap().executor)(&search);
        assert!(
            output(&value)
                .field("matches")
                .unwrap()
                .elements()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(journal).unwrap(),
            "PRIVATE_JOURNAL_CANARY"
        );
        assert_eq!(
            std::fs::read_to_string(evidence).unwrap(),
            "PRIVATE_EVIDENCE_CANARY"
        );
        assert!(!session.is_file());
    }

    #[test]
    fn registration_enforces_durable_admission_plan_filter_and_unknown_recovery() {
        let root = Root::new();
        let host = root.host();
        std::fs::write(root.path(), "old").unwrap();
        let args = root.args(&host, &root.path(), "old", "new");
        let snapshot = root.snapshot(&host);
        assert!(snapshot.requires_journal());
        assert!(snapshot.read_only().definition("edit_file").is_none());
        let policy = snapshot.policy("edit_file").unwrap();
        assert!(policy.requires_journal && !policy.read_only);
        let invocation = Invocation {
            operation: "fixture:op:1".into(),
            call: "call".into(),
            name: "edit_file".into(),
            revision: "ansa_edit_v1".into(),
            arguments: args.clone(),
        };
        assert_eq!(policy.recover.as_ref().unwrap()(&invocation).unwrap(), None);
        rejected(
            (snapshot.definition("edit_file").unwrap().executor)(&args),
            "durable_checkpoint_required",
        );
        let response = response(&host, &snapshot, &[args]);
        let mut ids = OperationIds::new("test_edit", 0).unwrap();
        let results = PreparedBatch::prepare(&response, snapshot, &mut ids)
            .unwrap()
            .execute(false);
        assert_eq!(results.results()[0].execution(), Execution::Denied);
        assert_eq!(results.results()[0].effects(), Effects::NotStarted);
        assert_eq!(std::fs::read_to_string(root.path()).unwrap(), "old");
    }

    #[test]
    fn a_rejected_later_call_prevents_the_entire_batch_from_editing() {
        let root = Root::new();
        let host = root.host();
        std::fs::write(root.path(), "old").unwrap();
        let valid = root.args(&host, &root.path(), "old", "new");
        let invalid = root.args(&host, &root.path(), "old", &"x".repeat(ARG_BYTES + 1));
        let snapshot = root.snapshot(&host);
        let response = response(&host, &snapshot, &[valid, invalid]);
        let mut ids = OperationIds::new("test_edit", 0).unwrap();
        assert!(matches!(
            PreparedBatch::prepare(&response, snapshot, &mut ids),
            Err("tool_arguments_rejected")
        ));
        assert_eq!(std::fs::read_to_string(root.path()).unwrap(), "old");
    }

    #[test]
    fn nested_instructions_must_be_acknowledged_before_editing() {
        let root = Root::new();
        let host = root.host();
        let directory = root.cwd().join("nested");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("AGENTS.md"), "Keep this source intact.").unwrap();
        let path = directory.join("file");
        std::fs::write(&path, "old").unwrap();
        let args = root.args(&host, &path, "old", "new");
        let first = host.edit(&args, &Cancel::default());
        assert_eq!(first.execution(), Execution::Failed);
        assert_eq!(first.effects(), Effects::KnownResult);
        let instructions = output(&first);
        assert_eq!(
            instructions.text_field("status").unwrap(),
            "instructions_required"
        );
        assert_eq!(*instructions.field("executed").unwrap(), b(false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        let acknowledged = args
            .with(
                "instruction_token",
                instructions.field("instruction_token").unwrap().clone(),
            )
            .unwrap();
        let outcome = host.edit(&acknowledged, &Cancel::default());
        assert_eq!(outcome.execution(), Execution::Succeeded);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
    }
}
