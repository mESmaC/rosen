//! Workspace lifecycle commands (SPEC.md R4, R10).
use crate::commands::WorkspaceCommand;
use rosen_client::{Endpoint, Error};

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    command: &WorkspaceCommand,
) -> Result<bool, Error> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    match command {
        WorkspaceCommand::List => {
            let records = client.workspace_list().await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"schema":crate::output::WORKSPACE_LIST_SCHEMA, "workspaces":records})
                );
            } else {
                for record in records {
                    match &record.kept {
                        Some(reason) => println!(
                            "{}  {}  (kept: {reason})",
                            record.change,
                            record.path.display()
                        ),
                        None => println!("{}  {}", record.change, record.path.display()),
                    }
                }
            }
        }
        WorkspaceCommand::Delete { workspace } => {
            client
                .workspace_delete(std::path::Path::new(workspace))
                .await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"schema":crate::output::WORKSPACE_DELETE_SCHEMA, "path":workspace})
                );
            }
        }
        WorkspaceCommand::Limits {
            materialisations,
            disk_gib,
        } => {
            let load = client
                .load_limits(
                    materialisations.map(|n| n as usize),
                    disk_gib.map(|gib| gib << 30),
                )
                .await?;
            print_limits(json, &load);
        }
        WorkspaceCommand::Watch { .. } => unreachable!("watch has its own event loop"),
    }
    Ok(true)
}

fn print_limits(json: bool, load: &rosen_client::workspace::limits::Load) {
    if json {
        println!(
            "{}",
            serde_json::json!({"schema":crate::output::WORKSPACE_LIMITS_SCHEMA, "limits":load})
        );
        return;
    }
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    println!(
        "materialisations  {} running of {}",
        load.running, load.settings.materialisations
    );
    println!(
        "disk              {:.1} GiB charged of {:.1} GiB",
        gib(load.charged_bytes),
        gib(load.settings.disk_bytes)
    );
}
