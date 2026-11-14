#![cfg(windows)]
//! R14 critical contract: a mismatched certificate must never receive the key;
//! enrollment limits, encrypted persistence, renewal and revocation protect access.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rosen_client::enrollment;
use rosen_protocol::{
    frame::{read_frame, write_frame},
    *,
};
use rosen_server::{
    machines,
    network::{self, tls, Identity},
    ServerConfig, ServerState,
};
use std::sync::Arc;
struct Project {
    dir: tempfile::TempDir,
    state: Arc<ServerState>,
    address: String,
    pin: [u8; 32],
}
async fn project() -> Project {
    let dir = tempfile::tempdir().unwrap();
    let identity = Identity::load_or_create(dir.path()).unwrap();
    let pin = identity.fingerprint();
    let acceptor = tls::acceptor(&identity).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: dir.path().into(),
        endpoint: Endpoint::new("unused-enrollment-test"),
        warm_pool_dir: None,
    });
    state.set_network(identity, address);
    tokio::spawn(network::serve(tcp, acceptor, Arc::clone(&state)));
    Project {
        dir,
        state,
        address: address.to_string(),
        pin,
    }
}
fn key(p: &Project) -> String {
    machines::keys::create(
        &p.state,
        CreateJoinKey {
            address: p.address.clone(),
            expires_at: machines::now() + 3600,
            use_limit: 1,
            role: MachineRole::Checker,
        },
    )
    .unwrap()
}
async fn connect(p: &Project, credential: Option<&str>) -> BoxStream {
    let mut stream = tls::connect(&p.address, p.pin).await.unwrap();
    write_frame(
        &mut stream,
        &ClientFrame::Hello {
            versions: SUPPORTED_VERSIONS.to_vec(),
            client: "test".into(),
            credential: credential.map(str::to_owned),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame::<_, ServerFrame>(&mut stream).await.unwrap(),
        Some(ServerFrame::Welcome { .. })
    ));
    stream
}
async fn request(stream: &mut BoxStream, request: Request) -> Option<ServerFrame> {
    write_frame(stream, &ClientFrame::Request { id: 1, request })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), read_frame(stream))
        .await
        .unwrap()
        .unwrap()
}
#[tokio::test]
#[cfg(windows)]
async fn headless_join_is_pinned_persisted_renewed_and_revocable() {
    let p = project().await;
    let machine = tempfile::tempdir().unwrap();
    let encoded = key(&p);
    let mut wrong: JoinKey =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&encoded).unwrap()).unwrap();
    wrong.fingerprint[0] ^= 1;
    let wrong = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wrong).unwrap());
    assert!(enrollment::join(machine.path(), &wrong, "headless")
        .await
        .is_err());
    let credential = enrollment::join(machine.path(), &encoded, "headless")
        .await
        .unwrap();
    assert!(enrollment::join(machine.path(), &encoded, "second")
        .await
        .is_err());
    verify_persistence(machine.path(), &credential);
    verify_access(&p, &credential).await;
    verify_rotation(&p, machine.path(), &credential).await;
    p.state.request_shutdown();
}
#[cfg(windows)]
fn verify_persistence(dir: &std::path::Path, credential: &MachineCredential) {
    let bytes = std::fs::read(dir.join("upstream.json")).unwrap();
    let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(!String::from_utf8(bytes)
        .unwrap()
        .contains(&credential.secret));
    assert_eq!(
        enrollment::resolve_reference(config["credential"].as_str().unwrap()).unwrap(),
        credential.secret
    );
}
async fn verify_access(p: &Project, credential: &MachineCredential) {
    let mut anonymous = connect(p, None).await;
    assert!(matches!(
        request(&mut anonymous, Request::Ping).await,
        Some(ServerFrame::Response {
            outcome: Outcome::Error(_),
            ..
        })
    ));
    let mut authenticated = connect(p, Some(&credential.secret)).await;
    assert!(matches!(
        request(&mut authenticated, Request::Ping).await,
        Some(ServerFrame::Response {
            outcome: Outcome::Ok(Response::Pong),
            ..
        })
    ));
}
#[cfg(windows)]
async fn verify_rotation(p: &Project, dir: &std::path::Path, credential: &MachineCredential) {
    let conn = machines::open(p.dir.path()).unwrap();
    // Move the server expiry into the renewal window, and seed the encrypted copy.
    let reference: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("upstream.json")).unwrap()).unwrap();
    let target = reference["credential"]
        .as_str()
        .unwrap()
        .strip_prefix("credman:")
        .unwrap();
    let mut stored: serde_json::Value =
        serde_json::from_slice(&rosen_platform::credentials::read(target).unwrap()).unwrap();
    stored["credential"]["expires_at"] = serde_json::json!(machines::now() + 3600);
    rosen_platform::credentials::write(target, &serde_json::to_vec(&stored).unwrap()).unwrap();
    conn.execute(
        "UPDATE machines SET expires_at=?1 WHERE id=?2",
        rusqlite::params![machines::now() + 3600, credential.machine_id],
    )
    .unwrap();
    enrollment::renew(dir).await.unwrap();
    let renewed = enrollment::resolve_reference(reference["credential"].as_str().unwrap()).unwrap();
    assert_ne!(renewed, credential.secret);
    let retry = machines::credentials::rotate(p.dir.path(), &credential.secret).unwrap();
    assert_eq!(retry.secret, renewed);
    let mut stream = connect(p, Some(&renewed)).await;
    conn.execute(
        "UPDATE machines SET status='revoked' WHERE id=?1",
        [&credential.machine_id],
    )
    .unwrap();
    assert!(request(&mut stream, Request::Ping).await.is_none());
    assert!(!machines::authenticate(p.dir.path(), &renewed).unwrap());
    rosen_platform::credentials::delete(target).unwrap();
}
