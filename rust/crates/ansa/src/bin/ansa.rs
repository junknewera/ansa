#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    eprintln!("{}", ansa::SCAFFOLD_MESSAGE);
    std::process::ExitCode::FAILURE
}
