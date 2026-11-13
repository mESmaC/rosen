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
                    serde_json::json!({"schema":"rosen.workspace.list/v1", "workspaces":records})
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
                    serde_json::json!({"schema":"rosen.workspace.delete/v1", "path":workspace})
                );
            }
        }
        WorkspaceCommand::Watch { .. } => unreachable!("watch has its own event loop"),
    }
    Ok(true)
}
