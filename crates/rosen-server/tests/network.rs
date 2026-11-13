//! Contract test for the network listener (SPEC R14).
//!
//! Risk covered: the listener exposes the whole project to the network. A
//! daemon that talks to a server with a certificate other than the pinned
//! one would hand its credential to an impostor, and a listener that admits
//! a peer without the credential lets anyone on the network change the
//! project.

use rosen_protocol::frame::{read_frame, write_frame};
use rosen_protocol::{ClientFrame, Endpoint, ServerFrame, SUPPORTED_VERSIONS};
use rosen_server::network::{self, tls, Identity};
use rosen_server::{ServerConfig, ServerState};
use std::sync::Arc;

async fn hello(address: &str, pin: network::Fingerprint, credential: Option<&str>) -> ServerFrame {
    let stream = tls::connect(address, pin).await.expect("pinned connect");
    let (mut reader, mut writer) = tokio::io::split(stream);
    let hello = ClientFrame::Hello {
        versions: SUPPORTED_VERSIONS.to_vec(),
        client: "test".into(),
        credential: credential.map(str::to_owned),
    };
    write_frame(&mut writer, &hello).await.unwrap();
    read_frame(&mut reader)
        .await
        .unwrap()
        .expect("an answer to hello")
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_pinned_server_and_a_credentialed_peer_get_through() {
    let dir = tempfile::tempdir().unwrap();
    let identity = Identity::load_or_create(dir.path()).unwrap();
    // A second load must return the same identity, or pinned daemons break.
    let reloaded = Identity::load_or_create(dir.path()).unwrap();
    assert_eq!(identity.fingerprint(), reloaded.fingerprint());
    let pin = identity.fingerprint();

    let acceptor = tls::acceptor(&identity).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: dir.path().to_path_buf(),
        endpoint: Endpoint::new(format!("rosen-network-test-{}", std::process::id())),
    });
    state.set_network(identity, address);
    let key = rosen_server::machines::keys::create(
        &state,
        rosen_protocol::CreateJoinKey {
            address: address.to_string(),
            expires_at: rosen_server::machines::now() + 3600,
            use_limit: 1,
            role: rosen_protocol::MachineRole::Agent,
        },
    )
    .unwrap();
    use base64::Engine as _;
    let key: rosen_protocol::JoinKey = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(key)
            .unwrap(),
    )
    .unwrap();
    let credential = rosen_server::machines::credentials::enroll(dir.path(), &key.secret, "test")
        .unwrap()
        .secret;
    tokio::spawn(network::serve(tcp, acceptor, Arc::clone(&state)));
    let address = address.to_string();

    let mut wrong_pin = pin;
    wrong_pin[0] ^= 1;
    assert!(
        tls::connect(&address, wrong_pin).await.is_err(),
        "a mismatched certificate must fail the TLS handshake, before any hello"
    );
    assert!(matches!(
        hello(&address, pin, None).await,
        ServerFrame::Welcome { .. }
    ));
    assert!(matches!(
        hello(&address, pin, Some("guess")).await,
        ServerFrame::Reject { .. }
    ));
    assert!(matches!(
        hello(&address, pin, Some(&credential)).await,
        ServerFrame::Welcome { .. }
    ));
    state.request_shutdown();
}
