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
                    println!("{}  {}", record.change, record.path.display());
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
        WorkspaceCommand::Watch { .. } => unreachable!("watch has its own event loop"),
    }
    Ok(true)
}
