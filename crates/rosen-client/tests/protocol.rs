//! Contract tests for the client–server protocol (SPEC §1 "Transport", §3
//! "Protocol", R9 "Events").
//!
//! Risk covered: every Rosen feature rides on this protocol. A client that hangs
//! on a version mismatch or an unknown request, a response routed to the wrong
//! caller when requests run concurrently, or an event lost or repeated across a
//! reconnect would break every agent at once.

use rosen_client::protocol::frame::{read_frame, write_frame};
use rosen_client::protocol::{ClientFrame, ErrorCode, EventKind, Listener, ServerFrame};
use rosen_client::{Client, Endpoint, Error};
use rosen_server::{ServerConfig, ServerState};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Starts an in-process server on an endpoint no other test uses.
fn start_server() -> (Endpoint, Arc<ServerState>) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "rosen-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    );
    let endpoint = if cfg!(windows) {
        Endpoint::new(name)
    } else {
        Endpoint::new(
            std::env::temp_dir()
                .join(name)
                .join("s.sock")
                .to_string_lossy(),
        )
    };
    let listener = Listener::bind(&endpoint).expect("bind test endpoint");
    let state = ServerState::new(ServerConfig {
        data_dir: std::env::temp_dir(),
        endpoint: endpoint.clone(),
    });
    tokio::spawn(rosen_server::listener::serve(listener, Arc::clone(&state)));
    (endpoint, state)
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_version_is_rejected_not_hung() {
    let (endpoint, _state) = start_server();
    let stream = rosen_client::protocol::connect(&endpoint).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);
    let hello = ClientFrame::Hello {
        versions: vec![999],
        client: "future".into(),
        credential: None,
    };
    write_frame(&mut writer, &hello).await.unwrap();
    let reply: Option<ServerFrame> = read_frame(&mut reader).await.unwrap();
    assert!(matches!(reply, Some(ServerFrame::Reject { supported, .. }) if supported == vec![1]));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_gets_an_error_under_its_id() {
    let (endpoint, _state) = start_server();
    let stream = rosen_client::protocol::connect(&endpoint).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);
    let hello = ClientFrame::Hello {
        versions: vec![1],
        client: "test".into(),
        credential: None,
    };
    write_frame(&mut writer, &hello).await.unwrap();
    let _welcome: Option<ServerFrame> = read_frame(&mut reader).await.unwrap();
    let future_request =
        serde_json::json!({"type": "request", "id": 7, "request": {"method": "teleport"}});
    write_frame(&mut writer, &future_request).await.unwrap();
    match read_frame(&mut reader).await.unwrap() {
        Some(ServerFrame::Response {
            id: 7,
            outcome: rosen_client::protocol::Outcome::Error(error),
        }) => {
            assert_eq!(error.code, ErrorCode::UnknownRequest)
        }
        other => panic!("expected an error for request 7, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_on_one_connection_all_answer() {
    let (endpoint, _state) = start_server();
    let client = Arc::new(Client::connect(&endpoint).await.unwrap());
    let calls = (0..64).map(|_| {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.status().await })
    });
    for call in calls {
        call.await
            .unwrap()
            .expect("every concurrent request is answered");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn events_resume_from_cursor_without_gaps_or_repeats() {
    let (endpoint, state) = start_server();
    let publish = |n: u64| {
        state
            .events
            .publish(EventKind::Landed, None, serde_json::json!(n))
    };
    (1..=3).for_each(|n| {
        publish(n);
    });
    let first = Client::connect(&endpoint).await.unwrap();
    let mut stream = first.subscribe(Some(0)).await.unwrap();
    let seen: Vec<u64> = take_seqs(&mut stream, 3).await;
    assert_eq!(seen, vec![1, 2, 3]);
    drop((stream, first));

    (4..=5).for_each(|n| {
        publish(n);
    });
    let second = Client::connect(&endpoint).await.unwrap();
    let mut resumed = second.subscribe(Some(3)).await.unwrap();
    publish(6);
    assert_eq!(take_seqs(&mut resumed, 3).await, vec![4, 5, 6]);
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_answers_then_refuses_new_work() {
    let (endpoint, state) = start_server();
    let client = Client::connect(&endpoint).await.unwrap();
    client.shutdown().await.unwrap();
    assert!(state.is_shutting_down());
    let late = client.ping().await;
    assert!(matches!(
        late,
        Err(Error::Disconnected) | Err(Error::Io(_)) | Err(Error::Server(_))
    ));
}

async fn take_seqs(
    stream: &mut tokio::sync::mpsc::Receiver<rosen_client::protocol::Event>,
    count: usize,
) -> Vec<u64> {
    let mut seqs = Vec::new();
    while seqs.len() < count {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), stream.recv())
            .await
            .expect("event within 5 s")
            .expect("stream open");
        seqs.push(event.seq);
    }
    seqs
}
