use super::protocol::{Binding, VERSION};
use ansa_linux::{fs, local};
use std::{fs::File, os::unix::fs::MetadataExt, path::Path};

const MARKER: &str = "active-terminal-scope.json";

/// An advisory writer lease for the files covered by one canonical workspace.
/// A crashed host leaves its marker for explicit reconciliation even after
/// Linux releases its lock. Close must have a durable known receipt first.
pub(super) struct Claim {
    _workspace: File,
    pub identity: (u64, u64),
    directory: File,
    _lock: File,
    marker: Vec<u8>,
}

impl Claim {
    pub(super) fn acquire(registry: &Path, binding: &Binding) -> Result<Self, &'static str> {
        if registry.starts_with(&binding.workspace)
            || !registry.is_absolute()
            || fs::normalize(registry) != registry
        {
            return Err("terminal_workspace_registry");
        }
        let workspace = fs::open_nofollow(Path::new(&binding.workspace), local::read_flags())
            .map_err(|_| "terminal_workspace_identity")?;
        let metadata = workspace
            .metadata()
            .map_err(|_| "terminal_workspace_identity")?;
        if !metadata.is_dir() {
            return Err("terminal_workspace_identity");
        }
        let identity = (metadata.dev(), metadata.ino());
        let path = registry.join(format!("{:x}-{:x}", identity.0, identity.1));
        fs::create_private(&path).map_err(|_| "terminal_workspace_registry")?;
        let directory = fs::private_directory(&path).map_err(|_| "terminal_workspace_registry")?;
        let lock = local::lock(&directory).map_err(|_| "terminal_workspace_busy")?;
        if local::read(&directory, MARKER, 8192)
            .map_err(|_| "terminal_workspace_evidence")?
            .is_some()
        {
            return Err("terminal_workspace_quarantined");
        }
        let marker = serde_json::to_vec(&serde_json::json!({"version":VERSION,"binding":binding}))
            .map_err(|_| "terminal_workspace_evidence")?;
        local::create(&directory, MARKER, &marker).map_err(|_| "terminal_workspace_evidence")?;
        Ok(Self {
            _workspace: workspace,
            identity,
            directory,
            _lock: lock,
            marker,
        })
    }

    pub(super) fn release(&self) -> Result<(), &'static str> {
        if local::read(&self.directory, MARKER, 8192)
            .map_err(|_| "terminal_workspace_evidence")?
            .as_deref()
            != Some(&self.marker)
        {
            return Err("terminal_workspace_evidence");
        }
        local::unlink(&self.directory, MARKER).map_err(|_| "terminal_workspace_evidence")
    }
}
