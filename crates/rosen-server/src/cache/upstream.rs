//! One TLS connection from a cache daemon to the server (SPEC D8, R14).
//!
//! Each local client connection gets its own link, so its forwarded
//! requests reach the server in the order the client sent them, and its
//! event stream is the server's stream for that client. Requests the cache
//! makes itself (object fetches) share the link under their own IDs.

use super::RequestGuard;
use crate::load_limits::{Charged, TransferLimits, TRANSFER_TIMEOUT};
use crate::network::{tls, UpstreamConfig};
use crate::SERVER_NAME;
use rosen_protocol::frame::{decode_body, read_frame, write_frame, write_frame_body};
use rosen_protocol::{
    BoxStream, ClientFrame, ErrorBody, ErrorCode, Outcome, Request, ServerFrame, SUPPORTED_VERSIONS,
};
use serde_json::Value;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};

/// Encoded frame bodies bound for the local client.
pub type Relay = mpsc::Sender<Charged<Vec<u8>>>;

/// Frames queued for the server. Bounded like the server's own outbox.
const OUTBOX_CAPACITY: usize = 16;

/// Who is waiting for the answer to a request sent upstream.
enum Waiter {
    /// The cache itself.
    Internal(oneshot::Sender<Outcome>),
    /// The local client, under the ID it chose, keeping shutdown waiting until
    /// its reply is queued (R10).
    Relay(u64, RequestGuard),
}

/// `None` once the link has closed, so no new waiter can be stranded.
type Pending = Arc<Mutex<Option<HashMap<u64, (Waiter, OwnedSemaphorePermit)>>>>;

pub struct UpstreamLink {
    frames: mpsc::Sender<Charged<Vec<u8>>>,
    pending: Pending,
    slots: Arc<Semaphore>,
    next_id: AtomicU64,
    subscribed: Arc<AtomicBool>,
    closed: watch::Receiver<bool>,
    /// Signalled on drop so the router keeps reading until every forwarded
    /// request is answered, then closes without dropping replies (R10).
    drain: watch::Sender<bool>,
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
        replies: Arc<TransferLimits>,
    ) -> io::Result<Self> {
        let stream = tls::connect(&config.address, config.fingerprint).await?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        // Bounded like the TLS handshake: LinkSlot holds its lock while
        // connecting, so a server that stalls here would hang the client.
        tokio::time::timeout(
            tls::HANDSHAKE_TIMEOUT,
            hello(
                &mut reader,
                &mut writer,
                &rosen_client::enrollment::resolve_reference(&config.credential)?,
            ),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no welcome from the server"))??;
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let subscribed = Arc::new(AtomicBool::new(false));
        let (closed_tx, closed) = watch::channel(false);
        let (drain, drain_rx) = watch::channel(false);
        let (frames, outbox) = mpsc::channel(OUTBOX_CAPACITY);
        let route = Router {
            pending: Arc::clone(&pending),
            relay,
            replies,
            subscribed: Arc::clone(&subscribed),
            lost,
            closed: closed_tx,
        };
        tokio::spawn(run_link(reader, writer, outbox, route, drain_rx));
        Ok(Self {
            frames,
            pending,
            slots: Arc::new(Semaphore::new(OUTBOX_CAPACITY)),
            next_id: AtomicU64::new(1),
            subscribed,
            closed,
            drain,
        })
    }

    pub fn is_open(&self) -> bool {
        !*self.closed.borrow()
    }

    /// Sends a request on the cache's own behalf and waits for its answer.
    pub async fn request(&self, request: Request) -> Outcome {
        let (sender, answer) = oneshot::channel();
        let Ok(id) = self.register(Waiter::Internal(sender)).await else {
            return unavailable_outcome();
        };
        let frame = ClientFrame::Request { id, request };
        let body = serde_json::to_vec(&frame).expect("a request always serialises");
        if self.frames.send(body.into()).await.is_err() {
            return unavailable_outcome();
        }
        answer.await.unwrap_or_else(|_| unavailable_outcome())
    }

    /// Forwards a client's request frame as it arrived, under a link ID; the
    /// answer goes back to the client under `client_id`. Returns `Err(in_flight)`
    /// when the link is closed and the client has not been answered.
    pub async fn forward(
        &self,
        client_id: u64,
        mut frame: Charged<Value>,
        in_flight: RequestGuard,
    ) -> Result<(), RequestGuard> {
        let id = match self.register(Waiter::Relay(client_id, in_flight)).await {
            Ok(id) => id,
            Err(Waiter::Relay(_, in_flight)) => return Err(in_flight),
            Err(Waiter::Internal(_)) => unreachable!("registered a relay waiter"),
        };
        frame.value["id"] = Value::from(id);
        let value = serde_json::to_vec(&frame.value).expect("a JSON value always serialises");
        if self
            .frames
            .send(Charged {
                value,
                permit: frame.permit,
            })
            .await
            .is_ok()
        {
            return Ok(());
        }
        // If the closing router already took the waiter, it answered the client.
        let removed = lock(&self.pending)
            .as_mut()
            .and_then(|waiting| waiting.remove(&id));
        match removed {
            Some((Waiter::Relay(_, in_flight), _)) => Err(in_flight),
            _ => Ok(()),
        }
    }

    /// Forwards the client's subscribe frame; events then flow to the relay.
    pub async fn subscribe(&self, body: Charged<Vec<u8>>) -> bool {
        self.subscribed.store(true, Ordering::SeqCst);
        self.frames.send(body).await.is_ok()
    }

    async fn register(&self, waiter: Waiter) -> Result<u64, Waiter> {
        let Ok(slot) = self.slots.clone().acquire_owned().await else {
            return Err(waiter);
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut pending = lock(&self.pending);
        let Some(waiting) = pending.as_mut() else {
            return Err(waiter);
        };
        waiting.insert(id, (waiter, slot));
        Ok(id)
    }
}

impl Drop for UpstreamLink {
    fn drop(&mut self) {
        // Tell the router no new requests will be registered; it stays open
        // until every pending reply arrives or the local client closes.
        self.drain.send_replace(true);
    }
}

async fn run_link(
    reader: ReadHalf<BoxStream>,
    writer: WriteHalf<BoxStream>,
    outbox: mpsc::Receiver<Charged<Vec<u8>>>,
    route: Router,
    drain: watch::Receiver<bool>,
) {
    {
        let mut write_done = false;
        let mut write = std::pin::pin!(write_upstream(writer, outbox));
        let mut read = std::pin::pin!(route.read_bodies(reader, drain));
        loop {
            tokio::select! {
                ok = &mut write, if !write_done => {
                    if !ok {
                        break;
                    }
                    // All queued frames were flushed; keep reading replies.
                    write_done = true;
                }
                _ = &mut read => break,
            }
        }
    }
    route.close().await;
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
    writer: WriteHalf<BoxStream>,
    outbox: mpsc::Receiver<Charged<Vec<u8>>>,
) {
    let _ = write_upstream(writer, outbox).await;
}

/// Writes queued frames to the peer. Returns true when `outbox` closed after
/// every queued frame was sent, and false on a write error or timeout.
async fn write_upstream(
    mut writer: WriteHalf<BoxStream>,
    mut outbox: mpsc::Receiver<Charged<Vec<u8>>>,
) -> bool {
    while let Some(body) = outbox.recv().await {
        let written =
            tokio::time::timeout(TRANSFER_TIMEOUT, write_frame_body(&mut writer, &body.value));
        if !matches!(written.await, Ok(Ok(()))) {
            return false;
        }
    }
    true
}

/// Routes the server's frames: answers to their waiters, events to the client.
struct Router {
    pending: Pending,
    relay: Relay,
    replies: Arc<TransferLimits>,
    subscribed: Arc<AtomicBool>,
    lost: Arc<watch::Sender<bool>>,
    closed: watch::Sender<bool>,
}

impl Router {
    fn is_idle(&self) -> bool {
        lock(&self.pending).as_ref().is_none_or(HashMap::is_empty)
    }

    async fn read_bodies(&self, mut reader: ReadHalf<BoxStream>, mut drain: watch::Receiver<bool>) {
        loop {
            let draining = *drain.borrow_and_update();
            if draining && self.is_idle() {
                return;
            }
            let read = tokio::select! {
                biased;
                _ = self.relay.closed() => return,
                changed = drain.changed(), if !draining => {
                    if (changed.is_err() || *drain.borrow_and_update()) && self.is_idle() {
                        return;
                    }
                    continue;
                }
                read = self.replies.read_body(&mut reader) => read,
            };
            match read {
                Ok(Some(body)) => {
                    let draining = *drain.borrow();
                    self.route(body, draining).await;
                }
                Ok(None) | Err(_) => return,
            }
        }
    }

    async fn close(self) {
        let stranded = lock(&self.pending).take().unwrap_or_default();
        for (waiter, _) in stranded.into_values() {
            // Internal waiters wake when their sender drops here.
            if let Waiter::Relay(client_id, in_flight) = waiter {
                let _ = self
                    .relay
                    .send(unavailable_response(client_id).into())
                    .await;
                drop(in_flight);
            }
        }
        self.closed.send_replace(true);
        if self.subscribed.load(Ordering::SeqCst) {
            self.lost.send_replace(true);
        }
    }

    async fn route(&self, body: Charged<Vec<u8>>, draining: bool) {
        let Ok(mut frame) = decode_body::<Value>(&body.value) else {
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
                    Some((Waiter::Internal(sender), _slot)) => {
                        let _ = sender.send(internal_outcome(&body.value));
                    }
                    Some((Waiter::Relay(client_id, in_flight), _slot)) => {
                        frame["id"] = Value::from(client_id);
                        let value = serde_json::to_vec(&frame).expect("JSON always serialises");
                        let _ = self
                            .relay
                            .send(Charged {
                                value,
                                permit: body.permit,
                            })
                            .await;
                        drop(in_flight);
                    }
                    None => {}
                }
            }
            // Events, and frame types newer than this build, pass through
            // until the link is draining for shutdown.
            Some("welcome") | Some("reject") => {}
            _ if draining => {}
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
