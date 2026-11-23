//! Contract test for R10 cache daemon shutdown draining (#476).
//!
//! Risk covered: dropping a stopping cache daemon's session aborts its
//! upstream link and loses the reply to a request the authoritative server
//! already accepted (such as a forwarded land), while leaving the local
//! listener open or waiting on an idle event subscriber breaks shutdown.

use rosen_client::{Client, Endpoint};
use rosen_core::AgentId;
use rosen_protocol::frame::{read_frame, write_frame};
use rosen_protocol::{
    ChangeFilter, ClientFrame, ErrorBody, ErrorCode, Outcome, Request, Response, ServerFrame,
};
use rosen_server::network::{self, tls, Identity};
use rosen_server::ServerConfig;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

fn cache_config(dir: &Path) -> ServerConfig {
    std::fs::create_dir_all(dir).unwrap();
    let endpoint = if cfg!(windows) {
        Endpoint::new(format!(
            "cache-shutdown-{}-{}",
            std::process::id(),
            dir.parent().unwrap().file_name().unwrap().to_string_lossy(),
        ))
    } else {
        Endpoint::new(dir.join("cache.sock").to_string_lossy())
    };
    ServerConfig {
        data_dir: dir.into(),
        endpoint,
        warm_pool_dir: None,
    }
}

async fn handle_upstream_peer(
    acceptor: tokio_rustls::TlsAcceptor,
    stream: tokio::net::TcpStream,
    received: Arc<Notify>,
    release: Arc<Notify>,
) {
    let Ok(mut tls) = acceptor.accept(stream).await else {
        return;
    };
    let Ok(Some(ClientFrame::Hello { .. })) = read_frame(&mut tls).await else {
        return;
    };
    let welcome = ServerFrame::Welcome {
        version: 1,
        server: "delayed-fixture".into(),
    };
    if write_frame(&mut tls, &welcome).await.is_err() {
        return;
    }
    while let Ok(Some(frame)) = read_frame::<_, ClientFrame>(&mut tls).await {
        match frame {
            ClientFrame::Request {
                id,
                request: Request::ChangeList(filter),
            } if filter.owner.as_ref().map(AgentId::as_str) == Some("drain-test") => {
                received.notify_one();
                release.notified().await;
                let reply = ServerFrame::Response {
                    id,
                    outcome: Outcome::Ok(Response::Changes(Vec::new())),
                };
                let _ = write_frame(&mut tls, &reply).await;
            }
            ClientFrame::Request { id, .. } => {
                let error = ErrorBody::new(ErrorCode::Unavailable, "ignored by fixture");
                let reply = ServerFrame::Response {
                    id,
                    outcome: Outcome::Error(error),
                };
                let _ = write_frame(&mut tls, &reply).await;
            }
            _ => {}
        }
    }
}

async fn start_delayed_upstream(
    dir: &Path,
    received: Arc<Notify>,
    release: Arc<Notify>,
) -> network::UpstreamConfig {
    let identity = Identity::load_or_create(dir).unwrap();
    let fingerprint = identity.fingerprint();
    let acceptor = tls::acceptor(&identity).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = tcp.accept().await {
            tokio::spawn(handle_upstream_peer(
                acceptor.clone(),
                stream,
                Arc::clone(&received),
                Arc::clone(&release),
            ));
        }
    });
    network::UpstreamConfig {
        address,
        fingerprint,
        credential: "test-credential".into(),
    }
}

async fn connect_ready(endpoint: &Endpoint) -> Client {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(client) = Client::connect(endpoint).await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_cache_drains_forwarded_requests_and_closes_listener() {
    let temp = tempfile::tempdir().unwrap();
    let (received, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let upstream = start_delayed_upstream(
        &temp.path().join("authority"),
        Arc::clone(&received),
        Arc::clone(&release),
    )
    .await;
    let config = cache_config(&temp.path().join("cache"));
    let endpoint = config.endpoint.clone();
    let mut cache =
        tokio::spawn(async move { rosen_server::cache::serve(&config, upstream).await });
    // An idle event subscriber must not block shutdown once requests finish.
    let subscriber = connect_ready(&endpoint).await;
    let _events = subscriber.subscribe(None).await.unwrap();
    let caller = connect_ready(&endpoint).await;
    let forwarded = tokio::spawn(async move {
        let filter = ChangeFilter {
            owner: Some(AgentId::new("drain-test").unwrap()),
            state: None,
        };
        caller.request(Request::ChangeList(filter)).await
    });
    tokio::time::timeout(Duration::from_secs(5), received.notified())
        .await
        .expect("cache forwarded request upstream");
    connect_ready(&endpoint).await.shutdown().await.unwrap();
    // Wait past the 2s flush window: cache must keep waiting for the reply
    // while refusing new local clients.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    assert!(!cache.is_finished(), "cache exited before upstream replied");
    assert!(Client::connect(&endpoint).await.is_err());
    release.notify_one();
    let answer = tokio::time::timeout(Duration::from_secs(5), forwarded)
        .await
        .expect("forwarded request answered")
        .unwrap()
        .expect("forwarded request succeeded");
    assert!(matches!(answer, Response::Changes(changes) if changes.is_empty()));
    let stopped = tokio::time::timeout(Duration::from_secs(5), &mut cache)
        .await
        .expect("cache exited once forwarded request was answered")
        .unwrap();
    assert!(stopped.is_ok());
}
