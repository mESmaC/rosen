//! A handshaken connection to the server, shared by any number of concurrent
//! requests. A background task reads every server frame and routes responses to
//! their waiting request by id, and events to the subscriber.

use crate::ClientError;
use rosen_protocol::frame::{read_frame, write_frame};
use rosen_protocol::{
    BoxStream, ClientFrame, Endpoint, Event, Outcome, Request, Response, ServerFrame, ServerStatus,
    SUPPORTED_VERSIONS,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot};

/// The client build name sent in the handshake.
pub const CLIENT_NAME: &str = concat!("rosen-client ", env!("CARGO_PKG_VERSION"));

/// Events buffered for a subscriber that has not read them yet. When it is full
/// the reader waits, which in turn slows the server's sends to this client.
const EVENT_BUFFER: usize = 1024;

/// Requests awaiting a response, or `None` once the connection has closed, so a
/// request made after the close fails at once instead of waiting forever.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Outcome>>>>>;
type EventSender = Arc<Mutex<Option<mpsc::Sender<Event>>>>;

pub struct Client {
    writer: tokio::sync::Mutex<WriteHalf<BoxStream>>,
    pending: Pending,
    events: EventSender,
    next_id: AtomicU64,
    version: u32,
    server: String,
}

impl Client {
    /// Connects and completes the handshake. Fails with
    /// [`ClientError::NotRunning`] when no server listens on `endpoint`.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self, ClientError> {
        let stream = rosen_protocol::connect(endpoint).await.map_err(|err| {
            if rosen_protocol::transport::is_not_running(&err) {
                ClientError::NotRunning(endpoint.clone())
            } else {
                ClientError::Io(err)
            }
        })?;
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (version, server) = handshake(&mut reader, &mut writer).await?;
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let events: EventSender = Arc::new(Mutex::new(None));
        tokio::spawn(route_frames(
            reader,
            Arc::clone(&pending),
            Arc::clone(&events),
        ));
        Ok(Self {
            writer: tokio::sync::Mutex::new(writer),
            pending,
            events,
            next_id: AtomicU64::new(1),
            version,
            server,
        })
    }

    /// The protocol version agreed in the handshake.
    pub fn protocol_version(&self) -> u32 {
        self.version
    }

    /// The server build named in the handshake.
    pub fn server_name(&self) -> &str {
        &self.server
    }

    /// Sends one request and waits for its response.
    pub async fn request(&self, request: Request) -> Result<Response, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        lock(&self.pending)
            .as_mut()
            .ok_or(ClientError::Disconnected)?
            .insert(id, answer);
        if let Err(err) = self.send(&ClientFrame::Request { id, request }).await {
            lock(&self.pending)
                .as_mut()
                .map(|waiting| waiting.remove(&id));
            return Err(err);
        }
        match answered.await.map_err(|_| ClientError::Disconnected)? {
            Outcome::Ok(response) => Ok(response),
            Outcome::Error(error) => Err(ClientError::Server(error)),
        }
    }

    /// Starts this connection's event stream from `after` (every retained event
    /// when `None`). Calling it again restarts the stream from the new cursor and
    /// closes the previous receiver.
    pub async fn subscribe(
        &self,
        after: Option<u64>,
    ) -> Result<mpsc::Receiver<Event>, ClientError> {
        let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
        *lock(&self.events) = Some(sender);
        self.send(&ClientFrame::Subscribe { after }).await?;
        Ok(receiver)
    }

    pub async fn ping(&self) -> Result<(), ClientError> {
        match self.request(Request::Ping).await? {
            Response::Pong => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub async fn status(&self) -> Result<ServerStatus, ClientError> {
        match self.request(Request::Status).await? {
            Response::Status(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    /// Asks the daemon to exit. It finishes answering open connections first.
    pub async fn shutdown(&self) -> Result<(), ClientError> {
        match self.request(Request::Shutdown).await? {
            Response::ShuttingDown => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    async fn send(&self, frame: &ClientFrame) -> Result<(), ClientError> {
        let mut writer = self.writer.lock().await;
        write_frame(&mut *writer, frame)
            .await
            .map_err(ClientError::Io)
    }
}

async fn handshake(
    reader: &mut ReadHalf<BoxStream>,
    writer: &mut WriteHalf<BoxStream>,
) -> Result<(u32, String), ClientError> {
    let hello = ClientFrame::Hello {
        versions: SUPPORTED_VERSIONS.to_vec(),
        client: CLIENT_NAME.to_owned(),
    };
    write_frame(writer, &hello).await.map_err(ClientError::Io)?;
    match read_frame::<_, ServerFrame>(reader)
        .await
        .map_err(ClientError::Io)?
    {
        Some(ServerFrame::Welcome { version, server }) => Ok((version, server)),
        Some(ServerFrame::Reject { reason, supported }) => {
            Err(ClientError::Rejected { reason, supported })
        }
        Some(other) => Err(ClientError::Unexpected(format!("{other:?} before welcome"))),
        None => Err(ClientError::Disconnected),
    }
}

/// Reads server frames until the connection ends, then fails every request
/// still waiting.
async fn route_frames(mut reader: ReadHalf<BoxStream>, pending: Pending, events: EventSender) {
    while let Ok(Some(frame)) = read_frame::<_, ServerFrame>(&mut reader).await {
        match frame {
            ServerFrame::Response { id, outcome } => {
                let waiter = lock(&pending)
                    .as_mut()
                    .and_then(|waiting| waiting.remove(&id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(outcome);
                }
            }
            ServerFrame::Event(event) => {
                let subscriber = lock(&events).clone();
                if let Some(subscriber) = subscriber {
                    // A dropped receiver only means nobody is listening any more.
                    let _ = subscriber.send(event).await;
                }
            }
            // Handshake frames after the handshake are ignored, not fatal, so a
            // misbehaving server cannot wedge the client.
            ServerFrame::Welcome { .. } | ServerFrame::Reject { .. } => {}
        }
    }
    // Dropping the waiters wakes each pending request with `Disconnected`.
    lock(&pending).take();
    lock(&events).take();
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn unexpected(response: Response) -> ClientError {
    ClientError::Unexpected(format!("unexpected response {response:?}"))
}
