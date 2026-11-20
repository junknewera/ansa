#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    match ansa::frontend::orchestration_pilot::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(code) => {
            eprintln!("{code}");
            std::process::ExitCode::FAILURE
        }
    }
}
