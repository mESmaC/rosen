//! Makes the server unreachable from one machine (SPEC.md §5).
//!
//! A [`PartitionProxy`] is a plain TCP relay on the server's machine, one per
//! other machine. That machine's daemon names the proxy as its `--upstream`,
//! so TLS and certificate pinning pass through untouched, and cutting the
//! proxy cuts exactly that machine off while the others keep the server. It
//! needs no firewall rule or administrator rights.

use crate::plan::Cut;
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// What the relay does with traffic right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Open,
    Cut(Cut),
}

pub struct PartitionProxy {
    address: SocketAddr,
    link: watch::Sender<Link>,
    accept: JoinHandle<()>,
}

impl PartitionProxy {
    /// Listens on `bind` (port 0 picks one) and relays every connection to
    /// `upstream`, the server's network listener.
    pub async fn start(bind: SocketAddr, upstream: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(bind).await?;
        let address = listener.local_addr()?;
        let (link, watching) = watch::channel(Link::Open);
        let accept = tokio::spawn(accept_loop(listener, upstream, watching));
        Ok(Self {
            address,
            link,
            accept,
        })
    }

    /// The address the machine's daemon should name as its upstream.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Cuts the machine off until [`PartitionProxy::heal`].
    pub fn cut(&self, cut: Cut) {
        self.link.send_replace(Link::Cut(cut));
    }

    /// Restores the link. Bytes a silent cut held back are delivered now,
    /// as TCP retransmission would after a route comes back.
    pub fn heal(&self) {
        self.link.send_replace(Link::Open);
    }

    pub fn is_cut(&self) -> bool {
        *self.link.borrow() != Link::Open
    }
}

impl Drop for PartitionProxy {
    fn drop(&mut self) {
        // Closing every relayed link too: each relay sees a refused cut.
        self.link.send_replace(Link::Cut(Cut::Refuse));
        self.accept.abort();
    }
}

async fn accept_loop(listener: TcpListener, upstream: SocketAddr, link: watch::Receiver<Link>) {
    loop {
        let Ok((peer, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(relay(peer, upstream, link.clone()));
    }
}

/// Relays one connection. A refused cut drops it, and a refused or silent
/// cut holds a new one before it reaches the server.
async fn relay(peer: TcpStream, upstream: SocketAddr, mut link: watch::Receiver<Link>) {
    match wait_passable(&mut link).await {
        Some(()) => {}
        None => return,
    }
    let Ok(server) = TcpStream::connect(upstream).await else {
        return;
    };
    let _ = peer.set_nodelay(true);
    let _ = server.set_nodelay(true);
    let (peer_read, peer_write) = peer.into_split();
    let (server_read, server_write) = server.into_split();
    let up = pump(peer_read, server_write, link.clone());
    let down = pump(server_read, peer_write, link);
    // Rosen's protocol never half-closes, so the first side to end ends the
    // link; waiting for both would leak an idle half after a reset.
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

/// Waits while the link is silently cut. `None` once it is refused: the
/// caller must drop its connection.
async fn wait_passable(link: &mut watch::Receiver<Link>) -> Option<()> {
    let state = *link
        .wait_for(|state| *state != Link::Cut(Cut::Silent))
        .await
        .ok()?;
    (state == Link::Open).then_some(())
}

/// Copies one direction. Each chunk waits for the link before it is
/// written, so a silent cut delays bytes and never loses or reorders them.
async fn pump(
    mut from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    mut link: watch::Receiver<Link>,
) -> io::Result<()> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = tokio::select! {
            read = from.read(&mut buffer) => read?,
            _ = link.wait_for(|state| *state == Link::Cut(Cut::Refuse)) => return Ok(()),
        };
        if read == 0 {
            return to.shutdown().await;
        }
        if wait_passable(&mut link).await.is_none() {
            return Ok(());
        }
        to.write_all(&buffer[..read]).await?;
    }
}
