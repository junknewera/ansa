//! Opt-in benchmark timings. The journal remains authoritative for outcomes.
use std::{
    cell::RefCell,
    io::Write,
    path::PathBuf,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static GENERATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn path() -> Option<&'static PathBuf> {
    PATH.get_or_init(|| std::env::var_os("ANSA_BENCHMARK_TRACE").map(PathBuf::from))
        .as_ref()
}

fn record(value: serde_json::Value) {
    if let Some(path) = path()
        && let Ok(mut file) = ansa_linux::fs::open_nofollow(path, ansa_linux::local::append_flags())
        && ansa_linux::fs::regular_single(&file).is_ok_and(|regular| regular)
    {
        let mut line = value.to_string();
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}

pub(crate) fn generation<T>(
    session: &str,
    phase: &'static str,
    run: impl FnOnce() -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    if path().is_none() {
        return run();
    }
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            GENERATION.with(|value| value.replace(self.0.take()));
        }
    }
    let id = format!(
        "{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let restore = Restore(GENERATION.with(|value| value.replace(Some(id.clone()))));
    let started = Instant::now();
    let result = run();
    let elapsed = started.elapsed().as_secs_f64();
    drop(restore);
    record(
        serde_json::json!({"kind":"generation","session":session,"id":id,"phase":phase,
                              "seconds":elapsed,"error":result.as_ref().err()}),
    );
    result
}

pub(crate) fn http<T>(run: impl FnOnce() -> Result<T, &'static str>) -> Result<T, &'static str> {
    if path().is_none() {
        return run();
    }
    let started = Instant::now();
    let result = run();
    let elapsed = started.elapsed().as_secs_f64();
    GENERATION.with(|id| {
        if id.borrow().is_some() {
            record(
                serde_json::json!({"kind":"http","id":*id.borrow(),"seconds":elapsed,
                                      "error":result.as_ref().err()}),
            );
        }
    });
    result
}

pub(crate) fn http_sent() {
    if path().is_some() {
        GENERATION.with(|id| {
            if id.borrow().is_some() {
                record(serde_json::json!({"kind":"http_sent","id":*id.borrow()}));
            }
        });
    }
}

pub(crate) fn tool<T>(operation: &str, run: impl FnOnce() -> T) -> T {
    if path().is_none() {
        return run();
    }
    let started = Instant::now();
    let result = run();
    record(serde_json::json!({"kind":"tool","operation":operation,
                              "seconds":started.elapsed().as_secs_f64()}));
    result
}
