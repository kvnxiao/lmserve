//! Prepare local model dependencies and coordinate recorded Podman deployments.

mod app;
mod artifacts;
mod cli;
mod config;
mod inputs;
mod lifecycle;
mod process;
mod runtime;
mod state;

use clap::Parser;
use std::process::ExitCode;

#[expect(
    clippy::print_stderr,
    reason = "CLI diagnostics report command failures"
)]
fn main() -> ExitCode {
    match app::run(cli::Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lmserve: {error:#}");
            ExitCode::FAILURE
        }
    }
}
