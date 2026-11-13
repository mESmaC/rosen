//! R14 critical contract: pending and revoked peers must never perform project work.
use rosen_protocol::{
    frame::{read_frame, write_frame},
    *,
};
use rosen_server::{
    machines,
    network::{self, tls, Identity},
    rpc, ServerConfig, ServerState,
};
use std::sync::Arc;

async fn request(stream: &mut BoxStream, request: Request) -> Outcome {
    write_frame(stream, &ClientFrame::Request { id: 1, request })
        .await
        .unwrap();
    match read_frame::<_, ServerFrame>(stream).await.unwrap().unwrap() {
        ServerFrame::Response { outcome, .. } => outcome,
        other => panic!("unexpected {other:?}"),
    }
}

async fn connect(address: &str, pin: [u8; 32], credential: Option<String>) -> BoxStream {
    let mut stream = tls::connect(address, pin).await.unwrap();
    write_frame(
        &mut stream,
        &ClientFrame::Hello {
            versions: SUPPORTED_VERSIONS.to_vec(),
            client: "management-test".into(),
            credential,
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

#[tokio::test]
async fn pending_approval_and_revocation_apply_to_the_next_request() {
    let (dir, state, pin, address, server) = project().await;
    let mut anonymous = connect(&address.to_string(), pin, None).await;
    let credential = approve_pending(&state, &mut anonymous).await;
    let mut enrolled = connect(&address.to_string(), pin, Some(credential.secret)).await;
    assert!(matches!(
        request(&mut enrolled, Request::Ping).await,
        Outcome::Ok(Response::Pong)
    ));
    check_presence(&state, &mut enrolled, &credential.machine_id).await;
    revoke(&state, &mut enrolled, &credential.machine_id).await;
    assert_eq!(
        machines::manage::list(&machines::open(dir.path()).unwrap())
            .unwrap()
            .len(),
        1
    );
    state.request_shutdown();
    server.await.unwrap();
}

async fn project() -> (
    tempfile::TempDir,
    Arc<ServerState>,
    [u8; 32],
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = ServerState::new(ServerConfig {
        data_dir: dir.path().into(),
        endpoint: Endpoint::new("unused-machine-test"),
    });
    let identity = Identity::load_or_create(dir.path()).unwrap();
    let pin = identity.fingerprint();
    let acceptor = tls::acceptor(&identity).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    state.set_network(identity, address);
    let server = tokio::spawn(network::serve(tcp, acceptor, Arc::clone(&state)));
    (dir, state, pin, address, server)
}

async fn approve_pending(state: &Arc<ServerState>, anonymous: &mut BoxStream) -> MachineCredential {
    let credential = match request(
        anonymous,
        Request::MachinePending {
            name: "headless".into(),
            role: MachineRole::Checker,
        },
    )
    .await
    {
        Outcome::Ok(Response::MachineCredential(value)) => value,
        other => panic!("unexpected {other:?}"),
    };
    assert!(matches!(
        request(anonymous, Request::MachineList).await,
        Outcome::Error(_)
    ));
    assert!(!machines::authenticate(&state.config.data_dir, &credential.secret).unwrap());
    let approved = rpc::dispatch(
        state,
        Request::MachineApprove {
            machine: credential.machine_id.clone(),
        },
    )
    .await;
    assert!(matches!(
        approved,
        Outcome::Ok(Response::Machine(MachineRecord {
            status: MachineStatus::Active,
            ..
        }))
    ));
    credential
}

async fn revoke(state: &Arc<ServerState>, enrolled: &mut BoxStream, id: &str) {
    assert!(matches!(
        rpc::dispatch(state, Request::MachineRevoke { machine: id.into() }).await,
        Outcome::Ok(Response::Machine(_))
    ));
    write_frame(
        enrolled,
        &ClientFrame::Request {
            id: 2,
            request: Request::Ping,
        },
    )
    .await
    .unwrap();
    // Refusal may close the authenticated session before dispatching the frame.
    assert!(matches!(
        read_frame::<_, ServerFrame>(enrolled).await.unwrap(),
        None | Some(ServerFrame::Response {
            outcome: Outcome::Error(_),
            ..
        })
    ));
    assert!(matches!(
        rpc::dispatch(state, Request::MachineApprove { machine: id.into() }).await,
        Outcome::Error(_)
    ));
}

async fn check_presence(state: &Arc<ServerState>, stream: &mut BoxStream, id: &str) {
    write_frame(
        stream,
        &ClientFrame::Subscribe {
            after: Some(0),
            subscriber: Some(rosen_core::AgentId::new("worker").unwrap()),
        },
    )
    .await
    .unwrap();
    // The following request is ordered after subscribe on this connection.
    let records = match request(stream, Request::MachineList).await {
        Outcome::Ok(Response::Machines(records)) => records,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        records.iter().find(|m| m.id == id).unwrap().running_agents,
        vec!["worker"]
    );
    let conn = machines::open(&state.config.data_dir).unwrap();
    assert!(machines::manage::keys(&conn).unwrap().is_empty());
}
