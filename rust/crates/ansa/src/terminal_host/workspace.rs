use super::protocol::Binding;
use ansa_linux::local;
use std::{os::unix::fs::MetadataExt, path::Path, sync::Arc};
mod recovery;
pub(crate) use recovery::cli;

/// Acquire write access, reconciling an old terminal lease when its evidence permits it.
pub fn writer(
    registry: &Path,
    workspace: &Path,
    target: &Path,
) -> Result<local::WorkspaceWrite, &'static str> {
    recovery::admit(registry, workspace, target, || {
        local::WorkspaceWrite::acquire(registry, workspace, target)
    })
}

pub(crate) fn plan_guard(settings: &crate::config::AgentSettings) -> crate::runtime::PlanGuard {
    let workspace = settings.cwd.clone();
    let registry = settings.directory.join("terminal-workspaces");
    Arc::new(move || {
        Ok(Box::new(recovery::admit(
            &registry,
            &workspace,
            &workspace,
            || local::workspace_participant(&registry, &workspace),
        )?))
    })
}

/// An advisory writer lease for the files covered by one canonical workspace.
/// A crashed host leaves its marker for explicit reconciliation even after
/// Linux releases its lock. Close must have a durable known receipt first.
pub(super) struct Claim {
    workspace: local::WorkspaceWrite,
    pub identity: (u64, u64),
    marker: Vec<u8>,
}

impl Claim {
    pub(super) fn acquire(
        registry: &Path,
        binding: &Binding,
        evidence: Option<&Path>,
    ) -> Result<Self, &'static str> {
        let path = Path::new(&binding.workspace);
        let mut workspace = writer(registry, path, path)?;
        if registry.starts_with(path) {
            return Err("terminal_workspace_registry");
        }
        let admitted = workspace
            .directory()
            .metadata()
            .map_err(|_| "terminal_workspace_identity")?;
        let identity = (admitted.dev(), admitted.ino());
        let marker = recovery::new_marker(binding, identity, evidence)?;
        workspace.start_terminal(&marker, &crate::protocol::legacy_json_v1::sha256(&marker))?;
        Ok(Self {
            workspace,
            identity,
            marker,
        })
    }

    pub(super) fn write_descendant(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<local::WorkspaceWrite, &'static str> {
        let root = ansa_linux::fs::open_nofollow(root, local::read_flags())
            .and_then(|directory| directory.metadata())
            .map_err(|_| "terminal_workspace_identity")?;
        if (root.dev(), root.ino()) != self.identity {
            return Err("terminal_workspace_identity");
        }
        self.workspace
            .write_descendant(relative, Some(&self.marker))
    }

    pub(super) fn release(self) -> Result<(), &'static str> {
        self.workspace.finish_terminal(&self.marker)
    }
}
