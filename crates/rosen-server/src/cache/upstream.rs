//! One TLS connection from a cache daemon to the server (SPEC D8, R14).
//!
//! Each local client connection gets its own link, so its forwarded
//! requests reach the server in the order the client sent them, and its
//! event stream is the server's stream for that client. Requests the cache
//! makes itself (object fetches) share the link under their own IDs.

use crate::network::{tls, UpstreamConfig};
use crate::SERVER_NAME;
use rosen_protocol::frame::{
    decode_body, read_frame, read_frame_body, write_frame, write_frame_body,
};
use rosen_protocol::{
    BoxStream, ClientFrame, ErrorBody, ErrorCode, Outcome, Request, ServerFrame, SUPPORTED_VERSIONS,
};
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

/// Encoded frame bodies bound for the local client.
pub type Relay = mpsc::Sender<Vec<u8>>;

/// Frames queued for the server. Bounded like the server's own outbox.
const OUTBOX_CAPACITY: usize = 256;

/// Who is waiting for the answer to a request sent upstream.
enum Waiter {
    /// The cache itself.
    Internal(oneshot::Sender<Outcome>),
    /// The local client, under the ID it chose.
    Relay(u64),
}

/// `None` once the link has closed, so no new waiter can be stranded.
type Pending = Arc<Mutex<Option<HashMap<u64, Waiter>>>>;

pub struct UpstreamLink {
    frames: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next_id: AtomicU64,
    subscribed: Arc<AtomicBool>,
    closed: watch::Receiver<bool>,
    tasks: [JoinHandle<()>; 2],
}

impl UpstreamLink {
    /// Connects, pins the server's certificate and presents the machine
    /// credential. Answers and events for the local client go to `relay`.
    /// `lost` is set if the link closes after the client subscribed, because
    /// the client's event stream cannot resume on a new link by itself.
    pub async fn connect(
        config: &UpstreamConfig,
        relay: Relay,
        lost: Arc<watch::Sender<bool>>,
    ) -> io::Result<Self> {
        let stream = tls::connect(&config.address, config.fingerprint).await?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        // Bounded like the TLS handshake: LinkSlot holds its lock while
        // connecting, so a server that stalls here would hang the client.
        tokio::time::timeout(
            tls::HANDSHAKE_TIMEOUT,
            hello(&mut reader, &mut writer, &config.credential),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no welcome from the server"))??;
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let subscribed = Arc::new(AtomicBool::new(false));
        let (closed_tx, closed) = watch::channel(false);
        let (frames, outbox) = mpsc::channel(OUTBOX_CAPACITY);
        let route = Router {
            pending: Arc::clone(&pending),
            relay,
            subscribed: Arc::clone(&subscribed),
            lost,
            closed: closed_tx,
        };
        let tasks = [
            tokio::spawn(write_bodies(writer, outbox)),
            tokio::spawn(route.run(reader)),
        ];
        Ok(Self {
            frames,
            pending,
            next_id: AtomicU64::new(1),
            subscribed,
            closed,
            tasks,
        })
    }

    pub fn is_open(&self) -> bool {
        !*self.closed.borrow()
    }

    /// Sends a request on the cache's own behalf and waits for its answer.
    pub async fn request(&self, request: Request) -> Outcome {
        let (sender, answer) = oneshot::channel();
        let id = match self.register(Waiter::Internal(sender)) {
            Some(id) => id,
            None => return unavailable_outcome(),
        };
        let frame = ClientFrame::Request { id, request };
        let body = serde_json::to_vec(&frame).expect("a request always serialises");
        if self.frames.send(body).await.is_err() {
            return unavailable_outcome();
        }
        answer.await.unwrap_or_else(|_| unavailable_outcome())
    }

    /// Forwards a client's request frame as it arrived, under a link ID; the
    /// answer goes back to the client under `client_id`. Returns false when
    /// the link is closed and the client has not been answered.
    pub async fn forward(&self, client_id: u64, mut frame: Value) -> bool {
        let Some(id) = self.register(Waiter::Relay(client_id)) else {
            return false;
        };
        frame["id"] = Value::from(id);
        let body = serde_json::to_vec(&frame).expect("a JSON value always serialises");
        if self.frames.send(body).await.is_ok() {
            return true;
        }
        // If the closing router already took the waiter, it answered the client.
        let still_waiting = lock(&self.pending)
            .as_mut()
            .and_then(|waiting| waiting.remove(&id))
            .is_some();
        !still_waiting
    }

    /// Forwards the client's subscribe frame; events then flow to the relay.
    pub async fn subscribe(&self, body: Vec<u8>) -> bool {
        self.subscribed.store(true, Ordering::SeqCst);
        self.frames.send(body).await.is_ok()
    }

    fn register(&self, waiter: Waiter) -> Option<u64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.pending).as_mut()?.insert(id, waiter);
        Some(id)
    }
}

impl Drop for UpstreamLink {
    fn drop(&mut self) {
        // The router holds the read half; stopping it closes the connection.
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn hello(
    reader: &mut ReadHalf<BoxStream>,
    writer: &mut WriteHalf<BoxStream>,
    credential: &str,
) -> io::Result<()> {
    let hello = ClientFrame::Hello {
        versions: SUPPORTED_VERSIONS.to_vec(),
        client: format!("{SERVER_NAME} (cache)"),
        credential: Some(credential.to_owned()),
    };
    write_frame(writer, &hello).await?;
    match read_frame::<_, ServerFrame>(reader).await? {
        Some(ServerFrame::Welcome { .. }) => Ok(()),
        Some(ServerFrame::Reject { reason, .. }) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the server refused this machine: {reason}"),
        )),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the server sent another frame before welcome",
        )),
        None => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}

pub(super) async fn write_bodies(
    mut writer: WriteHalf<BoxStream>,
    mut outbox: mpsc::Receiver<Vec<u8>>,
) {
    while let Some(body) = outbox.recv().await {
        if write_frame_body(&mut writer, &body).await.is_err() {
            return;
        }
    }
}

/// Routes the server's frames: answers to their waiters, events to the client.
struct Router {
    pending: Pending,
    relay: Relay,
    subscribed: Arc<AtomicBool>,
    lost: Arc<watch::Sender<bool>>,
    closed: watch::Sender<bool>,
}

impl Router {
    async fn run(self, mut reader: ReadHalf<BoxStream>) {
        while let Ok(Some(body)) = read_frame_body(&mut reader).await {
            self.route(body).await;
        }
        let stranded = lock(&self.pending).take().unwrap_or_default();
        for waiter in stranded.into_values() {
            // Internal waiters wake when their sender drops here.
            if let Waiter::Relay(client_id) = waiter {
                let _ = self.relay.send(unavailable_response(client_id)).await;
            }
        }
        self.closed.send_replace(true);
        if self.subscribed.load(Ordering::SeqCst) {
            self.lost.send_replace(true);
        }
    }

    async fn route(&self, body: Vec<u8>) {
        let Ok(mut frame) = decode_body::<Value>(&body) else {
            return;
        };
        match frame["type"].as_str() {
            Some("response") => {
                let Some(id) = frame["id"].as_u64() else {
                    return;
                };
                let waiter = lock(&self.pending)
                    .as_mut()
                    .and_then(|waiting| waiting.remove(&id));
                match waiter {
                    Some(Waiter::Internal(sender)) => {
                        let _ = sender.send(internal_outcome(&body));
                    }
                    Some(Waiter::Relay(client_id)) => {
                        frame["id"] = Value::from(client_id);
                        let body = serde_json::to_vec(&frame).expect("JSON always serialises");
                        let _ = self.relay.send(body).await;
                    }
                    None => {}
                }
            }
            // Events, and frame types newer than this build, pass through.
            Some("welcome") | Some("reject") => {}
            _ => {
                let _ = self.relay.send(body).await;
            }
        }
    }
}

fn internal_outcome(body: &[u8]) -> Outcome {
    match decode_body::<ServerFrame>(body) {
        Ok(ServerFrame::Response { outcome, .. }) => outcome,
        _ => Outcome::Error(ErrorBody::new(
            ErrorCode::Internal,
            "the server's answer could not be decoded by this cache",
        )),
    }
}

pub fn unavailable() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unavailable,
        "this machine's daemon could not reach the project's server",
    )
}

fn unavailable_outcome() -> Outcome {
    Outcome::Error(unavailable())
}

/// An error answer to the client's request `client_id`, already encoded.
pub fn unavailable_response(client_id: u64) -> Vec<u8> {
    error_response(client_id, unavailable())
}

pub fn error_response(client_id: u64, error: ErrorBody) -> Vec<u8> {
    let frame = ServerFrame::Response {
        id: client_id,
        outcome: Outcome::Error(error),
    };
    serde_json::to_vec(&frame).expect("a response always serialises")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}
