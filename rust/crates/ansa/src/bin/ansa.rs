#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    let args: Vec<String> = match std::env::args_os()
        .map(|arg| arg.into_string().map_err(|_| ()))
        .collect()
    {
        Ok(args) => args,
        Err(()) => {
            eprintln!(
                "{}",
                ansa::frontend::jsonl::diagnostic("json_string_invalid").dump()
            );
            return std::process::ExitCode::FAILURE;
        }
    };
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
    match ansa::frontend::app::dispatch(&args[1..]) {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(code) => {
            eprintln!(
                "{}",
                ansa::frontend::app::startup_diagnostic(&args[1..], code).dump()
            );
            std::process::ExitCode::FAILURE
        }
    }
}
