//! One client connection: the handshake, then concurrent requests and an
//! optional event stream sharing one ordered outgoing frame queue.
//!
//! Requests run as separate tasks so a slow request never delays a fast one;
//! the client matches answers by id. A single writer task owns the write half,
//! so frames from requests and events never interleave mid-frame.

use crate::events::{EventError, Page};
use crate::{rpc, ServerState, SERVER_NAME};
use rosen_core::AgentId;
use rosen_protocol::frame::{decode_body, read_frame, read_frame_body, write_frame};
use rosen_protocol::{
    negotiate_version, BoxStream, ClientFrame, ErrorBody, ErrorCode, Outcome, Request,
    RequestEnvelope, ServerFrame, SUPPORTED_VERSIONS,
};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

/// A client that connects but never says hello is dropped after this long, so
/// idle half-open connections cannot pile up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Frames queued for the writer. Bounded so a client that stops reading slows
/// its own requests and events instead of growing server memory.
const OUTBOX_CAPACITY: usize = 256;

/// Requests one connection may have running at once. The outbox alone does
/// not bound memory: a request whose answer cannot be queued still holds its
/// task. With this cap the reader stops taking frames once every slot is
/// busy, so a client that never reads its answers is pushed back through the
/// transport instead of making the server spawn a task per unread request.
const MAX_IN_FLIGHT: usize = 256;

/// Events read from the log per query, so a subscriber far behind catches
/// up in bounded steps instead of loading its whole backlog at once.
const EVENT_PAGE: u64 = 256;

/// Pause before retrying a failed event-log read.
const READ_RETRY: Duration = Duration::from_secs(1);

type Outbox = mpsc::Sender<ServerFrame>;

/// How a peer reached the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The local transport, already limited to the OS user.
    Local,
    /// The network listener (R14): the hello must carry a valid credential.
    Network,
}

/// Serves one local client until it disconnects, breaks the protocol, or
/// the server stops.
pub async fn serve_connection(stream: BoxStream, state: Arc<ServerState>) {
    serve_with_access(stream, state, Access::Local).await;
}

/// Serves one peer that arrived by `access`.
pub async fn serve_with_access(stream: BoxStream, state: Arc<ServerState>, access: Access) {
    let _open = state.track_connection();
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut presented = None;
    let admit = |credential: Option<&str>| match access {
        Access::Local => true,
        Access::Network => {
            presented = credential.map(str::to_owned);
            credential.is_none() || credential.is_some_and(|given| state.credential_matches(given))
        }
    };
    match handshake(&mut reader, &mut writer, admit).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            eprintln!("rosen-server: handshake failed: {err}");
            return;
        }
    }
    let (outbox, inbox) = mpsc::channel(OUTBOX_CAPACITY);
    let writer_task = tokio::spawn(write_frames(writer, inbox));
    let mut session = Session {
        state,
        access,
        credential: presented,
        outbox,
        in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        subscription: None,
        machine_agent: None,
    };
    session.read_frames(&mut reader).await;
    session.close();
    // The writer ends once every queued frame is written and every sender (this
    // session's and any in-flight request's) is gone.
    let _ = writer_task.await;
}

/// Reads the client's hello and answers it. `admit` decides whether the
/// hello's credential lets the peer in. Returns false when the client was
/// refused or left before saying hello.
pub(crate) async fn handshake(
    reader: &mut ReadHalf<BoxStream>,
    writer: &mut WriteHalf<BoxStream>,
    admit: impl FnOnce(Option<&str>) -> bool,
) -> io::Result<bool> {
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame::<_, ClientFrame>(reader))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no hello within the timeout"))??;
    let reply = match hello {
        None => return Ok(false),
        Some(ClientFrame::Hello { credential, .. }) if !admit(credential.as_deref()) => {
            reject("the machine credential is missing or not accepted".to_owned())
        }
        Some(ClientFrame::Hello { versions, .. }) => match negotiate_version(&versions) {
            Some(version) => ServerFrame::Welcome {
                version,
                server: SERVER_NAME.to_owned(),
            },
            None => reject(format!(
                "no common protocol version; client offered {versions:?}"
            )),
        },
        Some(_) => reject("the first frame must be hello".to_owned()),
    };
    let accepted = matches!(reply, ServerFrame::Welcome { .. });
    write_frame(writer, &reply).await?;
    Ok(accepted)
}

fn reject(reason: String) -> ServerFrame {
    ServerFrame::Reject {
        reason,
        supported: SUPPORTED_VERSIONS.to_vec(),
    }
}

async fn write_frames(mut writer: WriteHalf<BoxStream>, mut inbox: mpsc::Receiver<ServerFrame>) {
    while let Some(frame) = inbox.recv().await {
        if write_frame(&mut writer, &frame).await.is_err() {
            // The client is gone; dropping the receiver fails every pending send.
            return;
        }
    }
}

struct Session {
    machine_agent: Option<(String, String)>,
    state: Arc<ServerState>,
    access: Access,
    outbox: Outbox,
    /// One permit per running request; see [`MAX_IN_FLIGHT`].
    in_flight: Arc<Semaphore>,
    credential: Option<String>,
    subscription: Option<JoinHandle<()>>,
}

impl Session {
    async fn read_frames(&mut self, reader: &mut ReadHalf<BoxStream>) {
        loop {
            // Take a request slot before reading, so a client with every slot
            // busy is not read from at all until one of its requests ends.
            let slot = Arc::clone(&self.in_flight).acquire_owned();
            let permit = tokio::select! {
                permit = slot => permit,
                _ = self.state.shutdown_requested() => return,
            };
            // The session never closes its semaphore, so this cannot fail.
            let Ok(permit) = permit else { return };
            let read = tokio::select! {
                read = read_frame_body(reader) => read,
                _ = self.state.shutdown_requested() => return,
            };
            let body = match read {
                Ok(Some(body)) => body,
                Ok(None) => return,
                Err(err) => {
                    eprintln!("rosen-server: dropping client after read error: {err}");
                    return;
                }
            };
            if !self.handle_frame(&body, permit).await {
                return;
            }
        }
    }

    /// Acts on one frame. A request keeps `permit` until it is answered; any
    /// other frame releases it. Returns false when the client broke the
    /// protocol and the connection should close.
    async fn handle_frame(&mut self, body: &[u8], permit: OwnedSemaphorePermit) -> bool {
        if self.access == Access::Network
            && self
                .credential
                .as_deref()
                .is_some_and(|c| !self.state.credential_matches(c))
        {
            return false;
        }
        match decode_body::<ClientFrame>(body) {
            // Only the server's own machine may stop it or use its warm pool
            // (R3: each daemon's pool serves its own machine). A cache answers
            // these for its clients itself, so this only refuses a stray peer.
            Ok(ClientFrame::Request { id, request })
                if self.access == Access::Network && machine_local(&request) =>
            {
                let refused = ErrorBody::new(
                    ErrorCode::Refused,
                    "only the server's own machine may stop it or use its warm pool",
                );
                let response = ServerFrame::Response {
                    id,
                    outcome: Outcome::Error(refused),
                };
                self.outbox.send(response).await.is_ok()
            }
            Ok(ClientFrame::Request { id, request }) => {
                self.dispatch(id, request, permit);
                true
            }
            Ok(ClientFrame::Subscribe { after, subscriber }) => {
                if self.access == Access::Network
                    && !self
                        .credential
                        .as_deref()
                        .is_some_and(|c| self.state.credential_matches(c))
                {
                    return false;
                }
                self.subscribe(after, subscriber);
                true
            }
            Ok(ClientFrame::Hello { .. }) => false,
            Err(err) => self.answer_undecodable(body, &err.to_string()).await,
        }
    }

    fn dispatch(&self, id: u64, request: Request, permit: OwnedSemaphorePermit) {
        let state = Arc::clone(&self.state);
        let outbox = self.outbox.clone();
        let credential = self.credential.clone();
        let network = self.access == Access::Network;
        tokio::spawn(async move {
            let allowed = !network
                || match &request {
                    Request::MachineEnroll { .. }
                    | Request::MachineRotate { .. }
                    | Request::MachinePending { .. } => true,
                    _ => credential
                        .as_deref()
                        .is_some_and(|c| state.credential_matches(c)),
                };
            let outcome = if allowed {
                // A network peer is never the server's own machine, even
                // without a credential: then its machine is unknown.
                let origin = if network {
                    crate::receipts::Origin::Machine(credential.unwrap_or_default())
                } else {
                    crate::receipts::Origin::Local
                };
                rpc::dispatch_from(&state, request, origin).await
            } else {
                Outcome::Error(ErrorBody::new(
                    ErrorCode::Refused,
                    "machine credential required or revoked",
                ))
            };
            let _ = outbox.send(ServerFrame::Response { id, outcome }).await;
            // Freed only once the answer is queued, so a request blocked on a
            // full outbox still counts against the connection.
            drop(permit);
        });
    }

    /// A request this build cannot decode still gets an answer under its id, so
    /// a newer client can detect an older server instead of hanging.
    async fn answer_undecodable(&self, body: &[u8], reason: &str) -> bool {
        let Ok(RequestEnvelope {
            frame_type,
            id: Some(id),
        }) = decode_body::<RequestEnvelope>(body)
        else {
            return false;
        };
        if frame_type != "request" {
            return false;
        }
        // serde reports an unrecognised `method` tag as an unknown variant.
        let code = if reason.starts_with("unknown variant") {
            ErrorCode::UnknownRequest
        } else {
            ErrorCode::BadFrame
        };
        let error = Outcome::Error(ErrorBody::new(code, reason));
        let response = ServerFrame::Response { id, outcome: error };
        self.outbox.send(response).await.is_ok()
    }

    /// Starts (or restarts from a new cursor) this connection's event stream.
    fn subscribe(&mut self, after: Option<u64>, subscriber: Option<AgentId>) {
        if let Some((machine, nick)) = self.machine_agent.take() {
            crate::machines::manage::leave(&self.state, &machine, &nick);
        }
        if let (Some(credential), Some(agent)) = (&self.credential, &subscriber) {
            self.machine_agent =
                crate::machines::manage::join(&self.state, credential, &agent.to_string());
        }
        if let Some(previous) = self.subscription.take() {
            previous.abort();
        }
        let state = Arc::clone(&self.state);
        let outbox = self.outbox.clone();
        let stream = forward_events(state, outbox, after, subscriber);
        self.subscription = Some(tokio::spawn(stream));
    }

    fn close(mut self) {
        if let Some((machine, nick)) = self.machine_agent.take() {
            crate::machines::manage::leave(&self.state, &machine, &nick);
        }
        if let Some(subscription) = self.subscription.take() {
            subscription.abort();
        }
    }
}

/// Requests that act on the server's own machine rather than the project:
/// stopping the process, and its warm pool, whose takes rename directories
/// on the server's disk and whose size drives the server's disk use.
fn machine_local(request: &Request) -> bool {
    matches!(
        request,
        Request::Shutdown | Request::PoolSize { .. } | Request::PoolTake { .. }
    )
}

/// Sends every event after the starting cursor that the subscriber should
/// see, then each new one as it is published.
async fn forward_events(
    state: Arc<ServerState>,
    outbox: Outbox,
    after: Option<u64>,
    subscriber: Option<AgentId>,
) {
    let mut published = state.events.watch();
    let mut cursor = starting_cursor(&state, after, subscriber.clone()).await;
    loop {
        // Mark the current value seen before reading the log, so an event
        // published between the read and the wait still wakes this loop.
        published.borrow_and_update();
        let page = read_page(&state, cursor, subscriber.clone()).await;
        for event in page.events {
            if outbox.send(ServerFrame::Event(event)).await.is_err() {
                return;
            }
        }
        // Past every row the page scanned, including any this build could
        // not read, so a run of them cannot hold the stream back.
        cursor = page.scanned_to;
        // A full page means more may be waiting; read on without sleeping.
        if !page.full && published.changed().await.is_err() {
            return;
        }
    }
}

/// An explicit cursor wins. Otherwise a named subscriber resumes after the
/// last event it acknowledged, and anyone else starts from now.
async fn starting_cursor(
    state: &Arc<ServerState>,
    after: Option<u64>,
    subscriber: Option<AgentId>,
) -> u64 {
    if let Some(after) = after {
        return after;
    }
    retrying(|| {
        let state = Arc::clone(state);
        let subscriber = subscriber.clone();
        tokio::task::spawn_blocking(move || {
            let stored = match &subscriber {
                Some(agent) => state.events.cursor(agent)?,
                None => None,
            };
            Ok::<_, EventError>(stored.unwrap_or_else(|| state.events.last_seq()))
        })
    })
    .await
}

/// Reads the next page on the blocking pool: SQLite I/O must not stall the
/// async workers that carry every other connection.
async fn read_page(state: &Arc<ServerState>, cursor: u64, subscriber: Option<AgentId>) -> Page {
    retrying(|| {
        let state = Arc::clone(state);
        let subscriber = subscriber.clone();
        tokio::task::spawn_blocking(move || {
            state.events.page(cursor, subscriber.as_ref(), EVENT_PAGE)
        })
    })
    .await
}

/// Runs a log read until it succeeds. A failed read (a locked or briefly
/// unavailable database) is retried rather than ending the stream, because
/// the client would only see silence, never an end it could resume from.
/// The loop stops when the connection closes and aborts this task.
async fn retrying<T>(
    mut read: impl FnMut() -> tokio::task::JoinHandle<Result<T, EventError>>,
) -> T {
    loop {
        match read().await {
            Ok(Ok(value)) => return value,
            Ok(Err(err)) => eprintln!("rosen-server: retrying an event read: {err}"),
            Err(join) => eprintln!("rosen-server: retrying an event read: {join}"),
        }
        tokio::time::sleep(READ_RETRY).await;
    }
}
