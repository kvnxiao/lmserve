//! Reserve the CLI entry point for local model preparation and serving.

use std::process::ExitCode;

#[expect(
    clippy::print_stderr,
    reason = "CLI diagnostic reports unimplemented commands"
)]
fn main() -> ExitCode {
    eprintln!(
        "{}: CLI commands are not implemented yet",
        env!("CARGO_PKG_NAME")
    );
    ExitCode::FAILURE
}
