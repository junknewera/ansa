#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let Some(result) = ansa::process::runner::private_entry(&args) {
        return if result.is_ok() {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::from(125)
        };
    }
    if let Some(result) = ansa::process::supervisor::private_entry(&args) {
        return match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("m1_private_process_failed: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    eprintln!("{}", ansa::SCAFFOLD_MESSAGE);
    std::process::ExitCode::FAILURE
}
