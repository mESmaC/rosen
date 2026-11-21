//! `rosen server start|stop|status|relay` and `rosen events`: the commands that
//! talk to the daemon directly rather than through a project operation.

use rosen_client::protocol::{
    CollapseHealth, Event, FeedHealth, FeedState, RelayKind, RelaySetting, ServerStatus,
};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::AgentId;
use serde_json::json;

use crate::output::EVENTS_SCHEMA;
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

/// How long `stop` waits quietly before saying what it is waiting for.
const QUIET_STOP: std::time::Duration = std::time::Duration::from_secs(5);

/// Stops the running daemon and returns once its process has exited. Never
/// starts one: with none running there is nothing left to do, so that
/// succeeds too.
pub async fn stop(json: bool, endpoint: &Endpoint) -> Result<Succeeded, Error> {
    let stopping = rosen_client::stop_server(endpoint);
    tokio::pin!(stopping);
    let stopped = tokio::select! {
        stopped = &mut stopping => stopped?,
        () = tokio::time::sleep(QUIET_STOP) => {
            // Standard error stays empty with --json (R10).
            if !json {
                eprintln!("waiting for the server to answer its running requests and exit");
            }
            stopping.await?
        }
    };
    if json {
        let body = json!({
            "schema": crate::output::SERVER_STOP_SCHEMA,
            "endpoint": endpoint.as_str(),
            "was_running": stopped.is_some(),
            "pid": stopped.as_ref().map(|status| status.pid),
            "server": stopped.as_ref().map(|status| &status.server),
            "commit": stopped.as_ref().and_then(|status| status.commit.as_ref()),
        });
        println!("{body}");
    } else if let Some(status) = &stopped {
        println!("stopped: {} (pid {})", status.server, status.pid);
    } else {
        println!("not running ({endpoint})");
    }
    Ok(true)
}

fn print_status(json: bool, status: &ServerStatus) {
    if json {
        print_status_json(status);
    } else {
        print_status_text(status);
    }
}

fn print_status_json(status: &ServerStatus) {
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
        "mirror_proactive_auth": status.mirror_proactive_auth,
        "relay": status.relay,
        "collapse": status.collapse,
        "commit": status.commit,
        "executable": status.executable,
    });
    println!("{body}");
}

fn print_status_text(status: &ServerStatus) {
    println!("running: {} (pid {})", status.server, status.pid);
    if let Some(commit) = &status.commit {
        println!("commit: {commit}");
    }
    if let Some(executable) = &status.executable {
        println!("executable: {executable}");
    }
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
    if let Some(line) = mirror_owner_line(status.mirror_owner) {
        println!("{line}");
    }
    if status.mirror_proactive_auth == Some(true) {
        println!("mirror reads: proactive basic auth (http.proactiveAuth=basic)");
    }
    if let Some(relay) = &status.relay {
        println!(
            "collapse relay: {}",
            status_relay_line(relay, status.mirror_owner)
        );
    }
    if let Some(collapse) = &status.collapse {
        print_collapse_health(collapse);
    }
}

fn print_collapse_health(collapse: &CollapseHealth) {
    println!(
        "collapse state feed: {}",
        feed_health_line("seq", &collapse.state_feed)
    );
    println!(
        "collapse ledger push: {}",
        feed_health_line("seq", &collapse.ledger_push)
    );
    println!(
        "collapse action relay: {}",
        feed_health_line("action", &collapse.relay)
    );
}

fn feed_health_line(cursor_label: &str, health: &FeedHealth) -> String {
    let label = match health.state {
        FeedState::Sending => "sending",
        FeedState::Current => "current",
        FeedState::Failing => "failing",
        FeedState::Stopped => "stopped",
        FeedState::NotOwner => "not owner",
        FeedState::NoCredential => "no credential",
    };
    let mut parts = Vec::new();
    if let Some(code) = health.last_error.as_ref().and_then(|err| err.http_status) {
        parts.push(format!("HTTP {code}"));
    }
    if let Some(ledgers) = health.ledger_cursors.filter(|_| health.cursor.is_some()) {
        parts.push(format!(
            "discovery {}, past_work {}",
            ledgers.discovery, ledgers.past_work
        ));
    } else if let Some(cursor) = health.cursor {
        parts.push(format!("{cursor_label} {cursor}"));
    }
    if let Some(gen) = health.doorbell_generation {
        parts.push(format!("gen {gen}"));
    }
    if health.backlog > 0 {
        parts.push(format!("backlog {}", health.backlog));
    }
    if let Some(at) = health.last_success_at {
        parts.push(format!("last ok {}", utc_second(at)));
    }
    if let Some(at) = health.next_retry_at {
        parts.push(format!("retry {}", utc_second(at)));
    }
    let head = if parts.is_empty() {
        label.to_owned()
    } else {
        format!("{label} ({})", parts.join(", "))
    };
    match &health.last_error {
        Some(error) => {
            let message = error
                .message
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            format!("{head}: {message}")
        }
        None => head,
    }
}

fn utc_second(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}:{:02} UTC",
        crate::history_cmd::utc_date(seconds),
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

fn mirror_owner_line(mirror_owner: Option<bool>) -> Option<&'static str> {
    match mirror_owner {
        Some(true) => Some("mirror: pushes (mirror owner)"),
        Some(false) => Some("mirror: never pushes (not the mirror owner; Collapse state not sent)"),
        None => None,
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
                format!(
                    "unknown relay kind {word:?}; use approve, land, mark, config, revoke \
                     or none alone"
                )
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

fn status_relay_line(setting: &RelaySetting, mirror_owner: Option<bool>) -> String {
    if mirror_owner == Some(false) && setting.active() {
        let kinds: Vec<&str> = setting.allow.iter().map(|kind| kind.as_str()).collect();
        return format!("off (not the mirror owner; allows {})", kinds.join(", "));
    }
    relay_line(setting)
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

/// Idle timeout while draining pending events for `--once`.
const POLL_IDLE: std::time::Duration = std::time::Duration::from_millis(200);

/// Reads pending events up to the server's current sequence number and exits,
/// acknowledging them for `agent` when not `--all`.
pub async fn poll_events(
    json: bool,
    endpoint: &Endpoint,
    agent: Option<AgentId>,
    after: Option<u64>,
    limit: Option<usize>,
) -> Result<Succeeded, Error> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    let last_event = client.status().await?.last_event;
    let empty =
        limit == Some(0) || last_event == 0 || after.is_some_and(|cursor| cursor >= last_event);
    let mut collected = Vec::new();
    if !empty {
        let mut events = match &agent {
            Some(agent) => client.subscribe_as(agent.clone(), after).await?,
            None => client.subscribe(after).await?,
        };
        while let Ok(Some(event)) = tokio::time::timeout(POLL_IDLE, events.recv()).await {
            let seq = event.seq;
            collected.push(event);
            let hit_limit = limit.is_some_and(|max| collected.len() >= max);
            if seq >= last_event || hit_limit {
                break;
            }
        }
        if let (Some(agent), Some(last)) = (&agent, collected.last()) {
            client.ack_events(agent.clone(), last.seq).await?;
        }
    }
    print_events_batch(json, &collected, last_event);
    Ok(true)
}

fn print_events_batch(json: bool, events: &[Event], last_event: u64) {
    if json {
        let items: Vec<serde_json::Value> = events
            .iter()
            .map(|event| {
                json!({
                    "seq": event.seq,
                    "kind": event.kind,
                    "change": event.change,
                    "detail": event.detail,
                })
            })
            .collect();
        let body = json!({
            "schema": EVENTS_SCHEMA,
            "events": items,
            "last_event": last_event,
        });
        println!("{body}");
        return;
    }
    if events.is_empty() {
        println!("no new events (last event {last_event})");
        return;
    }
    for event in events {
        print_event(false, event);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Risk (#433): an operator inspecting a non-owner server needs to see
    /// directly that neither mirror pushes nor Collapse state or relay run.
    #[test]
    fn non_owner_status_reports_collapse_state_not_sent() {
        let mirror = mirror_owner_line(Some(false)).unwrap();
        assert!(
            mirror.contains("not the mirror owner; Collapse state not sent"),
            "{mirror}"
        );
        let setting = RelaySetting {
            allow: vec![RelayKind::Config, RelayKind::Approve],
            revoked: false,
        };
        assert_eq!(
            status_relay_line(&setting, Some(false)),
            "off (not the mirror owner; allows config, approve)"
        );
        assert_eq!(
            status_relay_line(&setting, Some(true)),
            "runs config, approve"
        );
    }

    /// Risk (#434): a revoked Collapse token must show HTTP 401 and the time
    /// of the last success on one line, then return cleanly to current.
    #[test]
    fn feed_health_line_reports_401_last_ok_and_current() {
        use rosen_client::protocol::FeedError;
        let failing = FeedHealth {
            state: FeedState::Failing,
            last_success_at: Some(1_700_000_000_000),
            cursor: Some(42),
            ledger_cursors: None,
            doorbell_generation: None,
            backlog: 3,
            last_error: Some(FeedError {
                http_status: Some(401),
                message: "HTTP 401: revoked\ntoken".into(),
                at: 1_700_000_060_000,
            }),
            next_retry_at: Some(1_700_000_065_000),
        };
        let line = feed_health_line("seq", &failing);
        assert!(!line.contains('\n'), "{line}");
        assert!(
            line.contains("failing (HTTP 401, seq 42, backlog 3, last ok 2023-11-14 22:13:20 UTC"),
            "{line}"
        );
        assert!(line.contains("retry 2023-11-14 22:14:25 UTC"), "{line}");

        let current = FeedHealth {
            state: FeedState::Current,
            last_success_at: Some(1_700_000_120_000),
            cursor: Some(45),
            ledger_cursors: None,
            doorbell_generation: None,
            backlog: 0,
            last_error: None,
            next_retry_at: None,
        };
        assert_eq!(
            feed_health_line("seq", &current),
            "current (seq 45, last ok 2023-11-14 22:15:20 UTC)"
        );
    }
}
