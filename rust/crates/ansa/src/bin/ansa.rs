#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
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
