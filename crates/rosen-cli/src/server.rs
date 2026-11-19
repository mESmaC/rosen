//! `rosen server start|status|relay` and `rosen events`: the commands that
//! talk to the daemon directly rather than through a project operation.

use rosen_client::protocol::{Event, RelayKind, RelaySetting, ServerStatus};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::AgentId;
use serde_json::json;

use crate::output::EVENT_SCHEMA;
use crate::output::SERVER_STATUS_SCHEMA as STATUS_SCHEMA;

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
            "listen": status.listen,
            "fingerprint": status.fingerprint,
            "upstream": status.upstream,
            "mirror_owner": status.mirror_owner,
            "relay": status.relay,
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
        if let Some(listen) = &status.listen {
            println!("network: {listen}");
        }
        if let Some(fingerprint) = &status.fingerprint {
            println!("fingerprint: {fingerprint}");
        }
        if let Some(upstream) = &status.upstream {
            println!("cache for: {upstream}");
        }
        match status.mirror_owner {
            Some(true) => println!("mirror: pushes (mirror owner)"),
            Some(false) => println!("mirror: never pushes (not the mirror owner)"),
            None => {}
        }
        if let Some(relay) = &status.relay {
            println!("collapse relay: {}", relay_line(relay));
        }
    }
}

/// Stores the relay's allow-list on the server's own machine, then prints
/// the setting now in force.
pub async fn relay(json: bool, endpoint: &Endpoint, allow: &[String]) -> Result<Succeeded, Error> {
    let kinds = relay_kinds(allow).map_err(Error::InvalidArgument)?;
    // Never start a daemon implicitly: an operator setting on a server that
    // was not running would mean the wrong endpoint was picked.
    let client = Client::connect(endpoint).await?;
    let setting = client.relay_set(kinds).await?;
    if json {
        let body = json!({ "schema": crate::output::SERVER_RELAY_SCHEMA, "relay": setting });
        println!("{body}");
    } else {
        println!("collapse relay: {}", relay_line(&setting));
    }
    Ok(true)
}

/// `none` alone, or kinds Rosen relays.
fn relay_kinds(allow: &[String]) -> Result<Vec<RelayKind>, String> {
    let words: Vec<&str> = allow.iter().map(|word| word.trim()).collect();
    if words == ["none"] {
        return Ok(Vec::new());
    }
    words
        .iter()
        .map(|word| {
            RelayKind::parse(word).ok_or_else(|| {
                format!("unknown relay kind {word:?}; use approve, land, mark or none alone")
            })
        })
        .collect()
}

fn relay_line(setting: &RelaySetting) -> String {
    let kinds: Vec<&str> = setting.allow.iter().map(|kind| kind.as_str()).collect();
    match (setting.revoked, kinds.is_empty()) {
        (true, _) => "off (the collapse machine is revoked)".to_owned(),
        (false, true) => "off".to_owned(),
        (false, false) => format!("runs {}", kinds.join(", ")),
    }
}

/// Prints events as they arrive until the server goes away. For an agent,
/// each batch is acknowledged once printed, so the next run resumes after it.
pub async fn stream_events(
    json: bool,
    endpoint: &Endpoint,
    agent: Option<AgentId>,
    after: Option<u64>,
) -> Result<Succeeded, Error> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    let mut events = match &agent {
        Some(agent) => client.subscribe_as(agent.clone(), after).await?,
        None => client.subscribe(after).await?,
    };
    while let Some(event) = events.recv().await {
        let mut last = event.seq;
        print_event(json, &event);
        // Print everything already buffered before acknowledging, so a busy
        // stream costs one acknowledgement per batch, not per event.
        while let Ok(event) = events.try_recv() {
            last = event.seq;
            print_event(json, &event);
        }
        if let Some(agent) = &agent {
            client.ack_events(agent.clone(), last).await?;
        }
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
