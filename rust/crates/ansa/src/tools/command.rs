use crate::{
    auth::Cancel,
    config::{n, path_text, s},
    protocol::{json::Json, legacy_json_v1::sha256},
    provider::replay::{Effects, Execution},
    tools::{evidence as v, invocation::Invocation, registry::Outcome},
};
use std::{
    fs::DirBuilder,
    os::unix::fs::DirBuilderExt,
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
        Json::object(vec![
            ("version", n(1)),
            ("invocation", invocation.save()?),
            ("cwd", s(path_text(&self.cwd)?)?),
            (
                "argv",
                Json::array(self.argv.iter().map(|v| s(v)).collect::<Result<_>>()?)?,
            ),
            ("timeout_ms", n(self.timeout_ms)),
            ("output_bytes", n(self.output_bytes)),
        ])
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
    pub fn new(root: &Path, worker: &Path) -> Result<Self> {
        let root = std::path::absolute(root).map_err(|_| "local_directory_path")?;
        ansa_linux::fs::create_private(&root).map_err(|_| "local_directory_permissions")?;
        let worker = worker.canonicalize().map_err(|_| "process_worker_path")?;
        Ok(Self { root, worker })
    }
    pub fn run(
        &self,
        invocation: &Invocation,
        options: &ProcessOptions,
        stop: &Cancel,
    ) -> Result<Outcome> {
        if stop.cancelled() {
            return Ok(Outcome::from_bytes(
                Execution::Cancelled,
                Effects::KnownResult,
                br#"{"error":"cancelled"}"#.to_vec(),
            ));
        }
        let spec = options.specification(invocation)?;
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
        let mut child = ansa_linux::supervisor::Supervisor::spawn(&self.worker, &path)
            .map_err(|_| "process_supervisor_spawn")?;
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
        v::admit(&directory, invocation)
    }
    pub fn recover(&self, invocation: &Invocation) -> Result<Option<Outcome>> {
        let path = self.root.join(sha256(invocation.operation.as_bytes()));
        if !path.exists() {
            return Ok(None);
        }
        let directory =
            ansa_linux::fs::private_directory(&path).map_err(|_| "local_directory_permissions")?;
        for _ in 0..60 {
            if v::read(&directory, "result.json")?.is_some() {
                return v::admit(&directory, invocation).map(Some);
            }
            sleep(Duration::from_millis(50));
        }
        Ok(None)
    }
}
