//! `rosen server start|status` and `rosen events`: the commands that talk to the
//! daemon directly rather than through a project operation.

use rosen_client::protocol::{Event, ServerStatus};
use rosen_client::{Client, Endpoint, Error};
use serde_json::json;

/// Schema tags for `--json` output (SPEC.md R10).
const STATUS_SCHEMA: &str = "rosen.server.status/v1";
const EVENT_SCHEMA: &str = "rosen.event/v1";

/// Whether a command succeeded, for the process exit status.
pub type Succeeded = bool;

/// Starts the daemon if needed, then prints its status.
pub async fn start(json: bool, endpoint: &Endpoint) -> Result<Succeeded, Error> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    print_status(json, &client.status().await?);
    Ok(true)
}

/// Prints the running daemon's status, or that none is running, without
/// starting one.
pub async fn status(json: bool, endpoint: &Endpoint) -> Result<Succeeded, Error> {
    match Client::connect(endpoint).await {
        Ok(client) => {
            print_status(json, &client.status().await?);
            Ok(true)
        }
        Err(Error::NotRunning(_)) => {
            if json {
                let body = json!({"schema": STATUS_SCHEMA, "running": false, "endpoint": endpoint.as_str()});
                println!("{body}");
            } else {
                println!("not running ({endpoint})");
            }
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

fn print_status(json: bool, status: &ServerStatus) {
    if json {
        let body = json!({
            "schema": STATUS_SCHEMA,
            "running": true,
            "server": status.server,
            "protocol_version": status.protocol_version,
            "pid": status.pid,
            "endpoint": status.endpoint,
            "data_dir": status.data_dir,
            "uptime_ms": status.uptime_ms,
            "open_connections": status.open_connections,
            "last_event": status.last_event,
        });
        println!("{body}");
    } else {
        println!("running: {} (pid {})", status.server, status.pid);
        println!("endpoint: {}", status.endpoint);
        println!("data dir: {}", status.data_dir);
        println!("protocol: v{}", status.protocol_version);
        println!("uptime: {:.1}s", status.uptime_ms as f64 / 1000.0);
        println!("connections: {}", status.open_connections);
        println!("last event: {}", status.last_event);
    }
}

/// Prints events as they arrive until the server goes away.
pub async fn stream_events(
    json: bool,
    endpoint: &Endpoint,
    after: Option<u64>,
) -> Result<Succeeded, Error> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    let mut events = client.subscribe(after).await?;
    while let Some(event) = events.recv().await {
        print_event(json, &event);
    }
    Err(Error::Disconnected)
}

fn print_event(json: bool, event: &Event) {
    if json {
        let body = json!({
            "schema": EVENT_SCHEMA,
            "seq": event.seq,
            "kind": event.kind,
            "change": event.change,
            "detail": event.detail,
        });
        println!("{body}");
    } else {
        let change = event.change.as_deref().unwrap_or("-");
        println!("{} {:?} {} {}", event.seq, event.kind, change, event.detail);
    }
}
