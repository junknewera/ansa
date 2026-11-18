use super::protocol::{Binding, VERSION};
use ansa_linux::local::{self, ACTIVE_TERMINAL_SCOPE as MARKER};
use std::{fs::File, os::unix::fs::MetadataExt, path::Path, sync::Arc};

pub(crate) fn plan_guard(settings: &crate::config::AgentSettings) -> crate::runtime::PlanGuard {
    let workspace = settings.cwd.clone();
    let registry = settings.directory.join("terminal-workspaces");
    Arc::new(move || {
        Ok(Box::new(local::workspace_participant(
            &registry, &workspace,
        )?))
    })
}

/// An advisory writer lease for the files covered by one canonical workspace.
/// A crashed host leaves its marker for explicit reconciliation even after
/// Linux releases its lock. Close must have a durable known receipt first.
pub(super) struct Claim {
    workspace: local::WorkspaceWrite,
    pub identity: (u64, u64),
    directory: File,
    marker: Vec<u8>,
}

impl Claim {
    pub(super) fn acquire(registry: &Path, binding: &Binding) -> Result<Self, &'static str> {
        let path = Path::new(&binding.workspace);
        let mut workspace = local::WorkspaceWrite::acquire(registry, path, path)?;
        let (_, directory, identity) =
            local::workspace_records(registry, path).map_err(|_| "terminal_workspace_registry")?;
        let admitted = workspace
            .directory()
            .metadata()
            .map_err(|_| "terminal_workspace_identity")?;
        if (admitted.dev(), admitted.ino()) != identity {
            return Err("terminal_workspace_identity");
        }
        let marker = serde_json::to_vec(&serde_json::json!({"version":VERSION,"binding":binding}))
            .map_err(|_| "terminal_workspace_evidence")?;
        local::create(&directory, MARKER, &marker).map_err(|_| "terminal_workspace_evidence")?;
        if let Err(error) = workspace.begin(&crate::protocol::legacy_json_v1::sha256(&marker)) {
            let _ = local::unlink(&directory, MARKER);
            return Err(error);
        }
        Ok(Self {
            workspace,
            identity,
            directory,
            marker,
        })
    }

    pub(super) fn release(self) -> Result<(), &'static str> {
        if local::read(&self.directory, MARKER, 8192)
            .map_err(|_| "terminal_workspace_evidence")?
            .as_deref()
            != Some(&self.marker)
        {
            return Err("terminal_workspace_evidence");
        }
        local::unlink(&self.directory, MARKER).map_err(|_| "terminal_workspace_evidence")?;
        self.workspace.finish()
    }
}
