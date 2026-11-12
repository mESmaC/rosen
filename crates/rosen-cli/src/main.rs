//! `rosen`: the Rosen command-line tool, a thin layer over `rosen-client`.

mod commands;

use std::process::ExitCode;

use clap::Parser;
use commands::Cli;

/// Schema tag for `--json` error output. Bump it when the shape changes, so
/// agents parsing the output can detect the change (SPEC.md R10).
const ERROR_SCHEMA: &str = "rosen.error/v1";

fn main() -> ExitCode {
    let cli = Cli::parse();
    let operation = cli.command.path();
    match rosen_client::run_operation(operation) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(cli.json, operation, &error);
            ExitCode::FAILURE
        }
    }
}

fn report_error(json: bool, operation: &str, error: &rosen_client::Error) {
    if json {
        let body = serde_json::json!({
            "schema": ERROR_SCHEMA,
            "command": operation,
            "error": error.to_string(),
        });
        println!("{body}");
    } else {
        eprintln!("rosen: {error}");
    }
}
