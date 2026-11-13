//! `rosen config show` (SPEC "Project configuration").

use crate::commands::ConfigCommand;
use crate::server::Succeeded;
use rosen_client::protocol::ConfigRecord;
use rosen_client::{Endpoint, Error};
use rosen_core::project_config::CheckSet;
use rosen_core::{ObjectId, ProjectConfig};
use serde_json::json;

/// Schema tag for `--json` output (SPEC R10). Bump on any shape change.
const CONFIG_SCHEMA: &str = "rosen.config/v2";

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    command: &ConfigCommand,
) -> Result<Succeeded, Error> {
    let ConfigCommand::Show { at } = command;
    // Parse arguments before starting a daemon, so a typo costs nothing.
    let at = at
        .as_deref()
        .map(|hex| {
            ObjectId::from_hex(hex)
                .map_err(|err| Error::InvalidArgument(format!("--at {hex:?}: {err}")))
        })
        .transpose()?;
    let client = rosen_client::connect_or_start(endpoint).await?;
    print_config(json, &client.project_config(at).await?);
    Ok(true)
}

fn print_config(json: bool, record: &ConfigRecord) {
    if json {
        println!("{}", json!({ "schema": CONFIG_SCHEMA, "config": record }));
        return;
    }
    match &record.base_ref {
        Some(base_ref) => println!("configuration at {} ({base_ref})", record.commit),
        None => println!("configuration at {}", record.commit),
    }
    match (&record.error, &record.source) {
        (Some(error), Some(source)) => {
            println!("warning: this commit's rosen.toml is refused: {error}");
            println!("in force instead: rosen.toml at {source}");
        }
        (Some(error), None) => {
            println!("warning: this commit's rosen.toml is refused: {error}");
            println!("in force instead: the defaults");
        }
        (None, None) => println!("no rosen.toml: the defaults are in force"),
        (None, Some(_)) => {}
    }
    print_parts(&record.config);
}

fn print_parts(config: &ProjectConfig) {
    print_patterns("generated files", &config.generated);
    print_patterns("registration lists", &config.registration_lists);
    print_patterns("sensitive paths", &config.sensitive);
    print_patterns("ignored paths", &config.ignore);
    print_build_units(config);
    print_check_set("pre-land check set", &config.checks.pre_land);
    print_check_set("post-land check set", &config.checks.post_land);
    println!("warm pool size: {}", config.warm_pool.size);
    print_patterns("prepare commands", &config.prepare.commands);
    match &config.mirror.remote {
        Some(remote) => println!("mirror remote: {remote}"),
        None => println!("mirror remote: (the one `rosen import --mirror` stored, if any)"),
    }
}

fn print_build_units(config: &ProjectConfig) {
    print_patterns("workspace-wide inputs (added)", &config.workspace_inputs);
    if config.build_units.is_empty() {
        println!("build units: discovered from the Cargo workspace");
        return;
    }
    println!("build units:");
    for (name, unit) in &config.build_units {
        let paths: Vec<String> = unit.paths.iter().map(ToString::to_string).collect();
        println!("  {name}: {}", paths.join(", "));
        if !unit.dependencies.is_empty() {
            println!("    depends on: {}", unit.dependencies.join(", "));
        }
    }
}

fn print_patterns(label: &str, patterns: &[impl std::fmt::Display]) {
    if patterns.is_empty() {
        println!("{label}: none");
        return;
    }
    println!("{label}:");
    for pattern in patterns {
        println!("  {pattern}");
    }
}

fn print_check_set(label: &str, set: &CheckSet) {
    println!("{label}: {}", set.name);
    if set.commands.is_empty() {
        println!("  (no commands configured)");
    }
    for command in &set.commands {
        println!("  {command}");
    }
}
