//! End-to-end check of the ledger push (issue #395, SPEC R13) against a
//! local endpoint that answers like Collapse's ledger intake, through the
//! real `curl` the server runs.
//!
//! Risk: the push is the only copy of the ledgers outside the server. A
//! push that skips an entry after an outage leaves the copy silently short,
//! and one that sends Velocity's versioned records or channel messages
//! leaks data the endpoint must never hold (and refuses whole batches for).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rosen_protocol::channel::{AppendEntry, Author, ChannelRequest, Intent, Ledger, SendMessage};
use rosen_protocol::Endpoint;
use rosen_server::ledger_push::{push_to, PushError, Pushed, Session};
use rosen_server::{channel, ServerConfig, ServerState};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the fake endpoint holds and how it answers.
#[derive(Default)]
struct Intake {
    /// Stored entries by (ledger, sequence): a resend changes nothing.
    held: BTreeMap<(String, u64), Value>,
    /// Every non-empty batch received, in order.
    batches: Vec<Vec<Value>>,
    /// The HTTP status to answer instead of storing, when set.
    failing: Option<u16>,
    /// When set, any batch whose body contains `needle` is refused with `status`.
    refuse: Option<(String, u16)>,
    refusals: usize,
}

impl Intake {
    fn highest(&self, ledger: &str) -> u64 {
        let keys = self.held.keys().filter(|(l, _)| l == ledger);
        keys.map(|(_, seq)| *seq).max().unwrap_or(0)
    }

    fn answer(&mut self, body: &[u8]) -> (u16, String) {
        if let Some(status) = self.failing {
            return (status, json!({"error": "unavailable"}).to_string());
        }
        if let Some((needle, status)) = &self.refuse {
            if String::from_utf8_lossy(body).contains(needle) {
                self.refusals += 1;
                return (*status, json!({"error": "invalid entry"}).to_string());
            }
        }
        let batch: Value = serde_json::from_slice(body).unwrap();
        let entries = batch["entries"].as_array().unwrap().clone();
        for entry in &entries {
            let ledger = entry["ledger"].as_str().unwrap().to_owned();
            let seq = entry["sequence"].as_u64().unwrap();
            self.held.entry((ledger, seq)).or_insert(entry.clone());
        }
        if !entries.is_empty() {
            self.batches.push(entries);
        }
        let highest = json!({
            "discovery": self.highest("discovery"),
            "past_work": self.highest("past_work"),
        });
        (200, json!({ "highest_sequence": highest }).to_string())
    }
}

/// Serves one HTTP/1.1 request per connection on a loopback port.
async fn serve(listener: tokio::net::TcpListener, intake: Arc<Mutex<Intake>>) {
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 8192];
        let (head_end, length) = loop {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                break (end + 4, length);
            }
        };
        while request.len() < head_end + length {
            let read = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
        }
        let (status, body) = intake.lock().unwrap().answer(&request[head_end..]);
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    }
}

fn append(state: &ServerState, ledger: Ledger, text: &str, velocity_issue: Option<&str>) {
    let entry = AppendEntry {
        ledger,
        author: Author::Agent { member: "a".into() },
        text: text.into(),
        velocity_issue: velocity_issue.map(str::to_owned),
    };
    channel::run(&state.events, ChannelRequest::Append(entry)).unwrap();
}

fn sent_sequences(intake: &Mutex<Intake>) -> Vec<u64> {
    let intake = intake.lock().unwrap();
    let all = intake.batches.iter().flatten();
    all.map(|entry| entry["sequence"].as_u64().unwrap())
        .collect()
}

#[test]
fn ledger_push_sends_only_ordinary_entries_and_catches_up_after_an_outage() {
    let data = tempfile::tempdir().unwrap();
    rosen_server::mirror::owner::mark(data.path()).unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.path().to_path_buf(),
        endpoint: Endpoint::new("rosen-ledger-push-test-unused"),
        warm_pool_dir: None,
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("http://{}/o/r/ledgers", listener.local_addr().unwrap());
    let intake = Arc::new(Mutex::new(Intake::default()));
    runtime.spawn(serve(listener, Arc::clone(&intake)));
    let push = |session: &mut Session| runtime.block_on(push_to(&state.config, &url, session));

    let join = ChannelRequest::Join {
        member: "a".into(),
        nick: "alice".into(),
        machine: "m1".into(),
    };
    channel::run(&state.events, join).unwrap();
    append(
        &state,
        Ledger::Discovery,
        "plain discovery",
        Some("velocity title"),
    );
    append(
        &state,
        Ledger::PastWork,
        r#"velocity-airc/v1:{"kind":"checkout"}"#,
        None,
    );
    append(&state, Ledger::PastWork, "landed the parser", None);
    let envelope = r#"velocity-airc/discovery/v1:{"text":"wrapped","rosen_issue":"rosen title"}"#;
    append(&state, Ledger::Discovery, envelope, None);
    append(
        &state,
        Ledger::PastWork,
        r#"{"kind":"task_result","ok":true}"#,
        None,
    );
    let message = SendMessage {
        from: Author::Agent { member: "a".into() },
        to: vec!["alice".into()],
        intent: Intent::Inform,
        text: "a channel message".into(),
    };
    channel::run(&state.events, ChannelRequest::Send(message)).unwrap();

    // Ordinary entries arrive in sequence order, each field as stored.
    let mut session = Session::default();
    let pushed = push(&mut session).unwrap();
    assert!(
        matches!(pushed, Pushed::Current { sent: 3, .. }),
        "{pushed:?}"
    );
    assert_eq!(sent_sequences(&intake), [1, 3, 4]);
    {
        let intake = intake.lock().unwrap();
        let first = &intake.batches[0];
        assert_eq!(first[0]["nick"], "alice");
        assert_eq!(first[0]["machine"], "m1");
        assert_eq!(first[0]["velocity_issue"], "velocity title");
        assert_eq!(first[2]["text"], "wrapped");
        assert_eq!(first[2]["rosen_issue"], "rosen title");
        assert!(first[2].get("velocity_issue").is_none());
        let keys = ["ledger", "sequence", "nick", "machine", "time", "text"];
        assert!(first[1]
            .as_object()
            .unwrap()
            .keys()
            .all(|k| keys.contains(&k.as_str())));
    }
    // Nothing new: nothing is sent again.
    push(&mut session).unwrap();
    assert_eq!(sent_sequences(&intake), [1, 3, 4]);

    // While the endpoint is down an append still commits, and the push
    // fails without moving its cursor.
    intake.lock().unwrap().failing = Some(503);
    append(&state, Ledger::Discovery, "written during the outage", None);
    let failed = push(&mut session).unwrap_err();
    assert!(
        matches!(
            failed,
            PushError::Send {
                status: Some(503),
                ..
            }
        ),
        "{failed:?}"
    );

    // The endpoint comes back having lost everything after sequence 1. The
    // next pass, like the task's after a failure, reads what it holds and
    // resends from there.
    {
        let mut intake = intake.lock().unwrap();
        intake.failing = None;
        intake.held.retain(|(_, seq), _| *seq <= 1);
    }
    let mut session = Session::default();
    push(&mut session).unwrap();
    // The intake keys entries by (ledger, sequence), so its keys come grouped by
    // ledger; the endpoint must hold these sequences, in whatever ledger.
    let mut held: Vec<u64> = intake.lock().unwrap().held.keys().map(|k| k.1).collect();
    held.sort_unstable();
    assert_eq!(held, [1, 3, 4, 6]);

    // An archived repository stops the push instead of being retried.
    intake.lock().unwrap().failing = Some(410);
    let mut session = Session::default();
    let gone = push(&mut session).unwrap_err();
    assert!(matches!(gone, PushError::Gone { .. }), "{gone:?}");
}

/// Risk (#444, R13): one entry Collapse refuses (400/413/422) must be split
/// out and skipped so valid entries deliver and the next pass does not resend
/// the refused entry.
#[test]
fn ledger_push_splits_refused_batch_and_skips_invalid_entry() {
    let data = tempfile::tempdir().unwrap();
    rosen_server::mirror::owner::mark(data.path()).unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: data.path().to_path_buf(),
        endpoint: Endpoint::new("rosen-ledger-refused-test"),
        warm_pool_dir: None,
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("http://{}/o/r/ledgers", listener.local_addr().unwrap());
    let intake = Arc::new(Mutex::new(Intake {
        refuse: Some(("poison-entry".into(), 422)),
        ..Intake::default()
    }));
    runtime.spawn(serve(listener, Arc::clone(&intake)));

    let join = ChannelRequest::Join {
        member: "a".into(),
        nick: "alice".into(),
        machine: "m1".into(),
    };
    channel::run(&state.events, join).unwrap();
    append(&state, Ledger::Discovery, "good first", None);
    append(&state, Ledger::Discovery, "poison-entry", None);
    append(&state, Ledger::PastWork, "good third", None);

    let mut session = Session::default();
    let pushed = runtime
        .block_on(push_to(&state.config, &url, &mut session))
        .unwrap();
    assert!(
        matches!(pushed, Pushed::Current { sent: 2, .. }),
        "{pushed:?}"
    );
    assert_eq!(sent_sequences(&intake), [1, 3]);

    // Next pass delivers a new entry without resending the refused entry.
    let refusals_before = intake.lock().unwrap().refusals;
    append(&state, Ledger::Discovery, "good fourth", None);
    let pushed = runtime
        .block_on(push_to(&state.config, &url, &mut session))
        .unwrap();
    assert!(
        matches!(pushed, Pushed::Current { sent: 1, .. }),
        "{pushed:?}"
    );
    assert_eq!(sent_sequences(&intake), [1, 3, 4]);
    assert_eq!(intake.lock().unwrap().refusals, refusals_before);
}
