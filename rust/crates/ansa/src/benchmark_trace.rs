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

pub(crate) fn unknown_response_event(ty: &str) {
    if path().is_none() {
        return;
    }
    // Only record names in the Responses namespace. Never include the event body.
    let valid = ty.starts_with("response.")
        && ty.len() <= 96
        && ty.split('.').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        });
    GENERATION.with(|slot| {
        if let Some(id) = slot.borrow().as_ref() {
            record(serde_json::json!({"kind":"unknown_response_event","id":id,
            "event_type": if valid { Some(ty) } else { None }}));
        }
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::OpenOptionsExt, process::Command};

    #[test]
    fn unknown_event_trace_contains_only_a_protocol_name_and_generation_id() {
        const CHILD: &str = "ANSA_TEST_EVENT_TRACE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            generation("test", "generate", || {
                unknown_response_event("response.example.delta");
                unknown_response_event("private_payload");
                unknown_response_event("response.example\nprivate text");
                Ok(())
            })
            .unwrap();
            return;
        }
        // The trace configuration is process-wide and is read once.
        let path = std::env::temp_dir().join(format!("ansa-event-trace-{}", std::process::id()));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        drop(file);
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "benchmark_trace::tests::unknown_event_trace_contains_only_a_protocol_name_and_generation_id"])
            .env(CHILD, "1").env("ANSA_BENCHMARK_TRACE", &path).status().unwrap();
        let content = fs::read_to_string(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert!(status.success());
        let rows: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 4);
        let id = rows[3]["id"].as_str().unwrap();
        assert!(!id.is_empty());
        assert_eq!(rows[3]["kind"], "generation");
        for (row, ty) in rows
            .iter()
            .zip([Some("response.example.delta"), None, None])
        {
            assert_eq!(
                *row,
                serde_json::json!({"kind":"unknown_response_event","id":id,"event_type":ty})
            );
        }
    }
}
