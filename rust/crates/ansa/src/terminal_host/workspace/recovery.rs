use crate::terminal_host::{
    FileJournal, Host, HostMode, OperationStatus, Tail,
    native::NativeBackend,
    protocol::{Binding, sha256},
};
use ansa_linux::{fs, local};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, &'static str>;
type Identity = (u64, u64);
const EVIDENCE: &str = "terminal_workspace_evidence";
const QUARANTINED: &str = "terminal_workspace_quarantined";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u16,
    binding: Binding,
    boot: Option<String>,
    workspace: Option<Identity>,
    store: Option<(String, Identity)>,
}

fn valid_boot(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn boot_id() -> Result<String> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|_| EVIDENCE)?;
    let id = value.trim();
    valid_boot(id).then(|| id.to_owned()).ok_or(EVIDENCE)
}

fn boot_time() -> Result<i64> {
    let mut stat = String::new();
    File::open("/proc/stat")
        .and_then(|f| f.take(2 * 1024 * 1024).read_to_string(&mut stat))
        .map_err(|_| EVIDENCE)?;
    stat.lines()
        .find_map(|line| line.strip_prefix("btime ")?.parse().ok())
        .ok_or(EVIDENCE)
}

fn identity(file: &File) -> Result<Identity> {
    let m = file.metadata().map_err(|_| EVIDENCE)?;
    Ok((m.dev(), m.ino()))
}

pub(super) fn new_marker(
    binding: &Binding,
    workspace: Identity,
    evidence: Option<&Path>,
) -> Result<Vec<u8>> {
    let store = evidence
        .map(|path| -> Result<_> {
            Ok((
                path.file_name()
                    .and_then(|s| s.to_str())
                    .ok_or(EVIDENCE)?
                    .to_owned(),
                identity(&fs::private_directory(path).map_err(|_| EVIDENCE)?)?,
            ))
        })
        .transpose()?;
    serde_json::to_vec(&Marker {
        version: 2,
        binding: binding.clone(),
        boot: Some(boot_id()?),
        workspace: Some(workspace),
        store,
    })
    .map_err(|_| EVIDENCE)
}

struct Claim {
    marker: Marker,
    bytes: Vec<u8>,
    token: String,
    ctime: i64,
}

impl Claim {
    fn read(workspace: &Path, root: &File, record: &File, boot: &str) -> Result<Option<Self>> {
        let Some(bytes) =
            local::read(record, local::ACTIVE_TERMINAL_SCOPE, 8192).map_err(|_| EVIDENCE)?
        else {
            return Ok(None);
        };
        let marker: Marker = serde_json::from_slice(&bytes).map_err(|_| EVIDENCE)?;
        let root_id = identity(root)?;
        if !marker.binding.validate()
            || Path::new(&marker.binding.workspace) != workspace
            || match marker.version {
                1 => marker.boot.is_some() || marker.workspace.is_some() || marker.store.is_some(),
                2 => {
                    marker.workspace != Some(root_id)
                        || !marker.boot.as_deref().is_some_and(valid_boot)
                }
                _ => true,
            }
        {
            return Err(EVIDENCE);
        }
        let stamp = local::terminal_marker_stamp(record).map_err(|_| EVIDENCE)?;
        let token = sha256(
            format!(
                "{}:{boot}:{}:{}:{}:{}:{}:{}",
                sha256(&bytes),
                root_id.0,
                root_id.1,
                stamp.dev(),
                stamp.ino(),
                stamp.ctime(),
                stamp.ctime_nsec()
            )
            .as_bytes(),
        );
        Ok(Some(Self {
            marker,
            bytes,
            token,
            ctime: stamp.ctime(),
        }))
    }

    fn predates(&self, boot: &str, since: i64) -> bool {
        match &self.marker.boot {
            Some(saved) => !saved.eq_ignore_ascii_case(boot),
            // The operator must independently confirm the reboot for a legacy claim.
            // Its wall time only rejects a claim apparently created on this boot.
            None => self.ctime < since,
        }
    }

    fn history(&self, registry: &Path) -> Result<Host<FileJournal, NativeBackend>> {
        let (name, store_id) = self.marker.store.as_ref().ok_or(QUARANTINED)?;
        let binding = &self.marker.binding;
        let conversation = registry
            .parent()
            .ok_or(EVIDENCE)?
            .join("sessions")
            .join(binding.session.as_str());
        let generation = super::super::tools::terminal_store_generation(&conversation)?;
        if generation == 0 {
            return Err(EVIDENCE);
        }
        let path = super::super::tools::terminal_store_path(&conversation, generation);
        if path.file_name().and_then(|value| value.to_str()) != Some(name)
            || *binding != Binding::attached(binding.session.clone(), binding.workspace.clone())
        {
            return Err(EVIDENCE);
        }
        let host = Host::restore_in_mode(
            &path,
            binding.clone(),
            NativeBackend::new(PathBuf::new(), registry.to_owned()),
            HostMode::Conversation,
        )?;
        if host.journal.identity()? != *store_id
            || !host.recovery().is_some_and(|r| r.tail == Tail::End)
            || host.operations.is_empty()
            || !host.operations.values().all(
                |op| matches!(op.status, OperationStatus::Durable(effect) if !effect.uncertain()),
            )
        {
            return Err(QUARANTINED);
        }
        Ok(host)
    }
}

fn inspect(registry: &Path, workspace: &Path, boot: &str) -> Result<Option<Claim>> {
    let (root, record, _) = local::workspace_records(registry, workspace).map_err(|_| EVIDENCE)?;
    Claim::read(workspace, &root, &record, boot)
}

fn covering_claim(registry: &Path, workspace: &Path, boot: &str) -> Result<Option<Claim>> {
    for candidate in workspace
        .ancestors()
        .take_while(|p| !registry.starts_with(p))
    {
        if let Some(claim) = inspect(registry, candidate, boot)? {
            return Ok(Some(claim));
        }
    }
    Ok(None)
}

fn release(
    registry: &Path,
    workspace: &Path,
    boot: &str,
    since: i64,
    claim: &Claim,
    explicit: bool,
) -> Result<()> {
    if !claim.predates(boot, since) || !explicit && claim.marker.boot.is_none() {
        return Err(QUARANTINED);
    }
    // Keep the validated terminal journal locked throughout automatic release.
    let _history = if explicit {
        None
    } else {
        Some(claim.history(registry)?)
    };
    local::retire_terminal_claim(
        registry,
        workspace,
        &sha256(&claim.bytes),
        |root, record| {
            let current = Claim::read(workspace, root, record, boot)?.ok_or(EVIDENCE)?;
            if current.token != claim.token {
                return Err(EVIDENCE);
            }
            Ok(current.bytes)
        },
    )
}

pub(super) fn admit<T>(
    registry: &Path,
    workspace: &Path,
    target: &Path,
    action: impl Fn() -> Result<T>,
) -> Result<T> {
    match action() {
        Err(QUARANTINED) => {
            let boot = boot_id().map_err(|_| QUARANTINED)?;
            let mut recovered = false;
            for path in [workspace, target] {
                if let Some(claim) =
                    covering_claim(registry, path, &boot).map_err(|_| QUARANTINED)?
                {
                    release(
                        registry,
                        Path::new(&claim.marker.binding.workspace),
                        &boot,
                        0,
                        &claim,
                        false,
                    )
                    .map_err(|_| QUARANTINED)?;
                    recovered = true;
                }
                if target == workspace {
                    break;
                }
            }
            if !recovered {
                return Err(QUARANTINED);
            }
            action()
        }
        result => result,
    }
}

pub(crate) fn cli(args: &[String], home: &Path) -> Result<String> {
    let workspace = std::env::current_dir().map_err(|_| EVIDENCE)?;
    let registry = home.join(".ansa/terminal-workspaces");
    let boot = boot_id()?;
    let since = boot_time()?;
    let claim = covering_claim(&registry, &workspace, &boot)?;
    match args {
        [] => {
            let Some(claim) = claim else {
                return Ok("No terminal lease for this workspace.".into());
            };
            let session = claim.marker.binding.session.as_str();
            let owner_workspace = &claim.marker.binding.workspace;
            if !claim.predates(&boot, since) {
                return Ok(format!(
                    "Terminal lease for session {session} belongs to this boot or cannot be dated before it."
                ));
            }
            Ok(format!(
                "Terminal lease for session {session} in {owner_workspace}. After confirming this machine rebooted since that terminal ran, release its local workspace lease with:\nansa --workspace-lease release-after-reboot {}\nHistorical operations retain their recorded outcomes.",
                claim.token
            ))
        }
        [action, token] if action == "release-after-reboot" => {
            let claim = claim.ok_or("terminal_workspace_marker_missing")?;
            if token != &claim.token {
                return Err("terminal_workspace_confirmation_mismatch");
            }
            release(
                &registry,
                Path::new(&claim.marker.binding.workspace),
                &boot,
                since,
                &claim,
                true,
            )?;
            Ok(
                "Released the local terminal lease. Existing operation receipts are preserved."
                    .into(),
            )
        }
        _ => Err("terminal_workspace_recovery_options"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_host::{
        Backend,
        protocol::{Action, Effect, Intent, Launch, Size, Writer},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PRIOR: &str = "00000000-0000-4000-8000-00000000000a";
    struct BackendResult(Effect);
    impl Backend for BackendResult {
        fn execute(&mut self, _: &Intent, _: &[u8]) -> Effect {
            self.0
        }
        fn stop(&mut self) {}
    }
    struct Fixture {
        base: PathBuf,
        workspace: PathBuf,
        registry: PathBuf,
        store: PathBuf,
        binding: Binding,
    }
    impl Fixture {
        fn new(effect: Effect) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let base = std::env::temp_dir().join(format!(
                "ansa-lease-reboot-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_private_leaf(&base).unwrap();
            let workspace = base.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let registry = base.join("state/terminal-workspaces");
            let session = "fixture".to_owned().try_into().unwrap();
            let binding = Binding::attached(session, workspace.to_str().unwrap().into());
            let conversation = base.join("state/sessions/fixture");
            fs::create_private(&conversation).unwrap();
            let store = conversation.join("terminal-host-v1");
            let mut host = Host::create_in_mode(
                &store,
                binding.clone(),
                BackendResult(effect),
                HostMode::Conversation,
            )
            .unwrap();
            let operation = "operation".to_owned().try_into().unwrap();
            let intent = Intent {
                terminal: binding.terminal_id(&operation),
                operation,
                binding: binding.clone(),
                writer: Writer::Agent,
                epoch: 0,
                invocation_digest: Some("a".repeat(64)),
                action: Action::Create {
                    launch: Launch {
                        label: "test".into(),
                        cwd: binding.workspace.clone(),
                        argv: vec!["sh".into()],
                        size: Size {
                            rows: 24,
                            columns: 80,
                        },
                        lifetime_seconds: 30,
                        idle_seconds: 5,
                    },
                },
            };
            assert_eq!(
                host.execute(Writer::Agent, intent, &[]),
                Ok(OperationStatus::Durable(effect))
            );
            Self {
                base,
                workspace,
                registry,
                store,
                binding,
            }
        }
        fn publish(&self, legacy: bool) -> (local::WorkspaceWrite, Claim) {
            let mut lease =
                local::WorkspaceWrite::acquire(&self.registry, &self.workspace, &self.workspace)
                    .unwrap();
            let (_, record, id) =
                local::workspace_records(&self.registry, &self.workspace).unwrap();
            let bytes = if legacy {
                serde_json::to_vec(&serde_json::json!({"version":1,"binding":self.binding}))
                    .unwrap()
            } else {
                let mut marker: Marker = serde_json::from_slice(
                    &new_marker(&self.binding, id, Some(&self.store)).unwrap(),
                )
                .unwrap();
                marker.boot = Some(PRIOR.into());
                serde_json::to_vec(&marker).unwrap()
            };
            local::create(&record, local::ACTIVE_TERMINAL_SCOPE, &bytes).unwrap();
            lease.begin(&sha256(&bytes)).unwrap();
            (
                lease,
                inspect(&self.registry, &self.workspace, &boot_id().unwrap())
                    .unwrap()
                    .unwrap(),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn prior_boot_with_known_receipts_allows_new_writer() {
        let fx = Fixture::new(Effect::Applied);
        let (active, claim) = fx.publish(false);
        let boot = boot_id().unwrap();
        assert_eq!(
            release(
                &fx.registry,
                &fx.workspace,
                &PRIOR.to_uppercase(),
                0,
                &claim,
                false
            ),
            Err(QUARANTINED)
        );
        assert_eq!(
            release(&fx.registry, &fx.workspace, &boot, 0, &claim, false),
            Err("terminal_workspace_busy")
        );
        let op = sha256(&claim.bytes);
        let mut pending = Vec::new();
        for ((dev, ino), root) in active.records() {
            let entry =
                fs::private_directory(&fx.registry.join(format!("{dev:x}-{ino:x}"))).unwrap();
            let prefix = if root {
                "workspace-root-writer-"
            } else {
                "workspace-writer-"
            };
            let file = format!("{prefix}{op}.v1.pending");
            local::create(&entry, &file, if root { b"" } else { &op.as_bytes()[..7] }).unwrap();
            pending.push((entry, file));
        }
        drop(active);
        let nested = fx.workspace.join("nested");
        std::fs::create_dir(&nested).unwrap();
        super::super::writer(&fx.registry, &nested, &nested)
            .unwrap()
            .finish()
            .unwrap();
        assert!(
            inspect(&fx.registry, &fx.workspace, &boot)
                .unwrap()
                .is_none()
        );
        assert!(
            pending
                .iter()
                .all(|(entry, name)| !local::entry_exists(entry, name).unwrap())
        );
    }

    #[test]
    fn unsealed_never_started_generation_reopens_for_new_writes_and_retains_receipts() {
        use crate::terminal_host::tools;
        let fx = Fixture::new(Effect::NotStarted);
        let mut settings = crate::config::AgentSettings::defaults(&fx.workspace, &fx.base);
        settings.directory = fx.base.join("state");
        let directory = fx.store.parent().unwrap();
        for expected_generation in [2, 3] {
            let owner = tools::open_for(&settings, "fixture", directory, true).unwrap();
            assert_eq!(
                tools::terminal_store_generation(directory),
                Ok(expected_generation)
            );
            let host = owner.lock().unwrap();
            assert!(host.recovery().is_none());
            assert_eq!(
                host.operation(&"operation".to_owned().try_into().unwrap()),
                Some(OperationStatus::Durable(Effect::NotStarted))
            );
            drop(host);
            assert!(
                tools::workspace_writer(&owner)(&fx.workspace)
                    .unwrap()
                    .is_none()
            );
            let mut write =
                super::super::writer(&fx.registry, &fx.workspace, &fx.workspace).unwrap();
            write.begin(&"d".repeat(64)).unwrap();
            std::fs::write(
                fx.workspace.join("new-file"),
                expected_generation.to_string(),
            )
            .unwrap();
            write.finish().unwrap();
        }
        assert_eq!(std::fs::read(fx.workspace.join("new-file")).unwrap(), b"3");
    }

    #[test]
    fn prior_boot_unknown_requires_exact_confirmation_and_preserves_receipt_and_other_writers() {
        let boot = boot_id().unwrap();
        for legacy in [false, true] {
            let fx = Fixture::new(Effect::Unknown);
            let (active, mut claim) = fx.publish(legacy);
            let mut nested = active
                .write_descendant(Path::new(""), Some(&claim.bytes))
                .unwrap();
            nested.begin(&"e".repeat(64)).unwrap();
            let other = nested.records().collect::<Vec<_>>();
            drop((active, nested));
            let since = claim.ctime + 1;
            let release_as = |claim: &Claim, since, explicit| {
                release(&fx.registry, &fx.workspace, &boot, since, claim, explicit)
            };
            assert_eq!(release_as(&claim, since, false), Err(QUARANTINED));
            if legacy {
                assert_eq!(release_as(&claim, claim.ctime, true), Err(QUARANTINED));
            }
            let token = std::mem::take(&mut claim.token);
            assert_eq!(release_as(&claim, since, true), Err(EVIDENCE));
            claim.token = token;
            release_as(&claim, since, true).unwrap();
            assert!(matches!(
                local::workspace_participant(&fx.registry, &fx.workspace),
                Err(QUARANTINED)
            ));
            let host = Host::restore_in_mode(
                &fx.store,
                fx.binding.clone(),
                BackendResult(Effect::Unknown),
                HostMode::Conversation,
            )
            .unwrap();
            assert_eq!(
                host.operation(&"operation".to_owned().try_into().unwrap()),
                Some(OperationStatus::Durable(Effect::Unknown))
            );
            assert!(host.quarantined());
            for (id, root) in other {
                local::reconcile_writer(&fx.registry, &"e".repeat(64), id, root).unwrap();
            }
            assert!(local::workspace_participant(&fx.registry, &fx.workspace).is_ok());
        }
    }
}
