#![cfg(windows)]

use rosen_protocol::transport::{connect, Endpoint, Listener};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Barrier};

// SPEC section 1, Transport: bursts must not force agents into offline fallback.
#[tokio::test]
async fn fifty_clients_connect_with_a_waiting_backlog() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let endpoint = Endpoint::new(format!("rosen-burst-{}-{nonce}", std::process::id()));
    let mut listener = Listener::bind(&endpoint).unwrap();
    tokio::time::timeout(Duration::from_secs(35), async {
        let barrier = Arc::new(Barrier::new(51));
        let (connected, mut arrivals) = mpsc::channel(50);
        let mut clients = Vec::new();
        for byte in 0..50u8 {
            let endpoint = endpoint.clone();
            let barrier = barrier.clone();
            let connected = connected.clone();
            clients.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut stream = connect(&endpoint).await.unwrap();
                connected.send(()).await.unwrap();
                stream.write_all(&[byte]).await.unwrap();
                let mut echoed = [0];
                stream.read_exact(&mut echoed).await.unwrap();
                assert_eq!(echoed, [byte]);
            }));
        }
        barrier.wait().await;
        // Eight clients must connect even before the accept loop is polled.
        for _ in 0..8 {
            arrivals.recv().await.unwrap();
        }
        for _ in 0..50 {
            let mut stream = listener.accept().await.unwrap();
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            stream.write_all(&byte).await.unwrap();
        }
        for client in clients {
            client.await.unwrap();
        }
    })
    .await
    .expect("all 50 clients must connect and exchange data");
}
