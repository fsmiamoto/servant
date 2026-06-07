use std::process::ExitCode;

fn main() -> ExitCode {
    let code = servant::cli::run();
    ExitCode::from(code as u8)
}
