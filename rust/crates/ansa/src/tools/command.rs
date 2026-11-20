use crate::{
    auth::Cancel,
    config::{n, path_text, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::replay::{Effects, Execution},
    tools::{evidence as v, invocation::Invocation, registry::Outcome},
};
use std::{
    fs::DirBuilder,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    thread::sleep,
    time::{Duration, Instant},
};
pub type Result<T> = v::Result<T>;
#[derive(Clone, Debug)]
pub struct ProcessOptions {
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub timeout_ms: usize,
    pub output_bytes: usize,
}
impl ProcessOptions {
    pub fn specification(&self, invocation: &Invocation) -> Result<Json> {
        self.specification_with_workspace(invocation, None)
    }

    fn specification_with_workspace(
        &self,
        invocation: &Invocation,
        workspace: Option<Json>,
    ) -> Result<Json> {
        if !self.cwd.is_absolute()
            || path_text(&self.cwd)?.contains('\0')
            || self.argv.is_empty()
            || self.argv.len() > 64
            || self.argv[0].is_empty()
            || !(1..=600000).contains(&self.timeout_ms)
            || !(1..=16384).contains(&self.output_bytes)
        {
            return Err("process_options");
        }
        let mut bytes = 0;
        for arg in &self.argv {
            if arg.contains('\0') || arg.len() > 16384 - bytes {
                return Err("process_arguments");
            }
            bytes += arg.len();
        }
        let mut fields = vec![
            ("version", n(1)),
            ("invocation", invocation.save()?),
            ("cwd", s(path_text(&self.cwd)?)?),
            (
                "argv",
                Json::array(self.argv.iter().map(|v| s(v)).collect::<Result<_>>()?)?,
            ),
            ("timeout_ms", n(self.timeout_ms)),
            ("output_bytes", n(self.output_bytes)),
        ];
        if let Some(workspace) = workspace {
            fields.push(("workspace_binding", workspace));
        }
        Json::object(fields)
    }
    pub(crate) fn load(spec: &Json) -> Result<Self> {
        let options = Self {
            cwd: spec.text_field("cwd")?.into(),
            argv: spec
                .field("argv")?
                .elements()?
                .iter()
                .map(|v| v.text().map(String::from))
                .collect::<Result<_>>()?,
            timeout_ms: spec.usize_field("timeout_ms")?,
            output_bytes: spec.usize_field("output_bytes")?,
        };
        options.specification(&Invocation::load(spec.field("invocation")?)?)?;
        Ok(options)
    }
}
#[derive(Clone)]
pub struct ProcessRunner {
    root: PathBuf,
    worker: PathBuf,
}
impl ProcessRunner {
    pub(crate) fn read_output(&self, args: &Json) -> Result<Json> {
        crate::process::output_spool::read_page(&self.root, args)
    }

    pub(crate) fn has_job(&self, invocation: &Invocation) -> Result<bool> {
        match self
            .root
            .join(sha256(invocation.operation.as_bytes()))
            .symlink_metadata()
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("process_job_metadata"),
        }
    }

    pub fn new(root: &Path, worker: &Path) -> Result<Self> {
        let runner = Self::configured(root, worker)?;
        ansa_linux::fs::create_private(&runner.root).map_err(|_| "local_directory_permissions")?;
        Ok(runner)
    }

    pub(crate) fn configured(root: &Path, worker: &Path) -> Result<Self> {
        let root = std::path::absolute(root).map_err(|_| "local_directory_path")?;
        let worker = worker.canonicalize().map_err(|_| "process_worker_path")?;
        Ok(Self { root, worker })
    }
    pub fn run(
        &self,
        invocation: &Invocation,
        options: &ProcessOptions,
        stop: &Cancel,
    ) -> Result<Outcome> {
        self.run_in(invocation, options, stop, None)
    }

    pub(crate) fn run_in(
        &self,
        invocation: &Invocation,
        options: &ProcessOptions,
        stop: &Cancel,
        mut workspace: Option<ansa_linux::local::WorkspaceWrite>,
    ) -> Result<Outcome> {
        if stop.cancelled() {
            return Ok(Outcome::from_bytes(
                Execution::Cancelled,
                Effects::KnownResult,
                br#"{"error":"cancelled"}"#.to_vec(),
            ));
        }
        let binding = if let Some(workspace) = &workspace {
            let directory = workspace
                .directory()
                .metadata()
                .map_err(|_| "terminal_workspace_identity")?;
            let number = |value: u64| Json::parse(&value.to_string());
            let scopes = workspace
                .records()
                .map(|((device, inode), root)| {
                    Json::object(vec![
                        ("device", number(device)?),
                        ("inode", number(inode)?),
                        ("root", Json::parse(if root { "true" } else { "false" })?),
                    ])
                })
                .collect::<Result<_>>()?;
            Some(Json::object(vec![
                ("version", n(1)),
                ("device", number(directory.dev())?),
                ("inode", number(directory.ino())?),
                ("scopes", Json::array(scopes)?),
            ])?)
        } else {
            None
        };
        let spec = options.specification_with_workspace(invocation, binding)?;
        let root = ansa_linux::fs::private_directory(&self.root)
            .map_err(|_| "local_directory_permissions")?;
        let path = self.root.join(sha256(invocation.operation.as_bytes()));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let directory = ansa_linux::fs::private_directory(&path)
                    .map_err(|_| "local_directory_permissions")?;
                if v::read(&directory, "request.json")?.as_ref() != Some(&spec) {
                    return Err("process_operation_conflict");
                }
                return Ok(self.recover(invocation)?.unwrap_or_else(v::unknown));
            }
            Err(_) => return Err("process_job_create"),
        }
        root.sync_all().map_err(|_| "local_sync")?;
        let directory =
            ansa_linux::fs::private_directory(&path).map_err(|_| "local_directory_permissions")?;
        v::create(&directory, "request.json", &spec)?;
        if let Some(workspace) = workspace.as_mut() {
            workspace.begin(&sha256(invocation.save()?.dump().as_bytes()))?;
        }
        let outcome = self.run_job(
            invocation,
            options,
            stop,
            &path,
            &directory,
            workspace.is_some(),
        );
        if outcome
            .as_ref()
            .is_ok_and(|outcome| outcome.effects() != Effects::Unknown)
            && let Some(workspace) = workspace
        {
            let _ = workspace.finish();
        }
        outcome
    }

    fn run_job(
        &self,
        invocation: &Invocation,
        options: &ProcessOptions,
        stop: &Cancel,
        path: &Path,
        directory: &std::fs::File,
        bound_required: bool,
    ) -> Result<Outcome> {
        let mut child = match ansa_linux::supervisor::Supervisor::spawn_with_directory(
            &self.worker,
            path,
            bound_required,
        ) {
            Ok(child) => child,
            // Once clone3 has returned a child PID, the supervisor only
            // emits custom identity errors. A system errno here proves that
            // no worker could have dispatched this request.
            Err(error) if error.raw_os_error().is_some() && bound_required => {
                let spec = v::read(directory, "request.json")?.ok_or("process_job_request")?;
                v::create(
                    directory,
                    "started.json",
                    &Json::object(vec![
                        ("version", n(1)),
                        ("request_hash", s(&sha256(spec.dump().as_bytes()))?),
                    ])?,
                )?;
                let outcome =
                    Outcome::failed_known(r#"{"error":"process_supervisor_spawn"}"#.into());
                v::publish(directory, &spec, &outcome)?;
                return Ok(outcome);
            }
            Err(_) => return Err("process_supervisor_spawn"),
        };
        let deadline = Instant::now()
            + Duration::from_millis(options.timeout_ms as u64)
            + Duration::from_secs(5);
        let mut resumed = false;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        return Ok(v::unknown());
                    }
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    child
                        .finish()
                        .map_err(|_| "process_supervisor_cleanup_unknown")?;
                    return Ok(v::unknown());
                }
            }
            if stop.cancelled() {
                child.control.take();
            }
            if Instant::now() >= deadline {
                if !resumed {
                    child.control.take();
                    child
                        .signal(ansa_linux::process::Signal::CONT)
                        .map_err(|_| "process_supervisor_signal")?;
                    resumed = true;
                    sleep(Duration::from_millis(1600));
                    continue;
                }
                child
                    .signal(ansa_linux::process::Signal::KILL)
                    .map_err(|_| "process_supervisor_signal")?;
                child
                    .finish()
                    .map_err(|_| "process_supervisor_cleanup_unknown")?;
                return Ok(v::unknown());
            }
            sleep(Duration::from_millis(10));
        }
        v::admit(directory, invocation)
    }
    pub fn recover(&self, invocation: &Invocation) -> Result<Option<Outcome>> {
        self.recover_in(invocation, None)
    }

    pub(crate) fn inspect_outcome(root: &Path, invocation: &Invocation) -> Result<Outcome> {
        let path = root.join(sha256(invocation.operation.as_bytes()));
        let directory =
            ansa_linux::fs::private_directory(&path).map_err(|_| "local_directory_permissions")?;
        v::admit(&directory, invocation)
    }

    pub(crate) fn recover_in(
        &self,
        invocation: &Invocation,
        registry: Option<&Path>,
    ) -> Result<Option<Outcome>> {
        let path = self.root.join(sha256(invocation.operation.as_bytes()));
        if !path.exists() {
            return Ok(None);
        }
        let directory =
            ansa_linux::fs::private_directory(&path).map_err(|_| "local_directory_permissions")?;
        for _ in 0..60 {
            if v::read(&directory, "result.json")?.is_some() {
                let outcome = v::admit(&directory, invocation)?;
                if outcome.effects() == Effects::KnownResult
                    && let Some(registry) = registry
                    && let Some(spec) = v::read(&directory, "request.json")?
                    && let Ok(binding) = spec.field("workspace_binding")
                {
                    let marker = sha256(invocation.save()?.dump().as_bytes());
                    for scope in binding.field("scopes")?.elements()? {
                        let _ = ansa_linux::local::reconcile_writer(
                            registry,
                            &marker,
                            (
                                scope.usize_field("device")? as u64,
                                scope.usize_field("inode")? as u64,
                            ),
                            scope.field("root")?.boolean()?,
                        );
                    }
                }
                return Ok(Some(outcome));
            }
            sleep(Duration::from_millis(50));
        }
        Ok(None)
    }
}
