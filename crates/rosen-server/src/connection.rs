//! One client connection: the handshake, then concurrent requests and an
//! optional event stream sharing one ordered outgoing frame queue.
//!
//! Requests run as separate tasks so a slow request never delays a fast one;
//! the client matches answers by id. A single writer task owns the write half,
//! so frames from requests and events never interleave mid-frame.

use crate::{rpc, ServerState, SERVER_NAME};
use rosen_protocol::frame::{decode_body, read_frame, read_frame_body, write_frame};
use rosen_protocol::{
    negotiate_version, BoxStream, ClientFrame, ErrorBody, ErrorCode, Outcome, RequestEnvelope,
    ServerFrame, SUPPORTED_VERSIONS,
};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A client that connects but never says hello is dropped after this long, so
/// idle half-open connections cannot pile up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Frames queued for the writer. Bounded so a client that stops reading slows
/// its own requests and events instead of growing server memory.
const OUTBOX_CAPACITY: usize = 256;

type Outbox = mpsc::Sender<ServerFrame>;

/// Serves one client until it disconnects, breaks the protocol, or the server stops.
pub async fn serve_connection(stream: BoxStream, state: Arc<ServerState>) {
    let _open = state.track_connection();
    let (mut reader, mut writer) = tokio::io::split(stream);
    match handshake(&mut reader, &mut writer).await {
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
        outbox,
        subscription: None,
    };
    session.read_frames(&mut reader).await;
    session.close();
    // The writer ends once every queued frame is written and every sender (this
    // session's and any in-flight request's) is gone.
    let _ = writer_task.await;
}

/// Reads the client's hello and answers it. Returns false when the client was
/// refused or left before saying hello.
async fn handshake(
    reader: &mut ReadHalf<BoxStream>,
    writer: &mut WriteHalf<BoxStream>,
) -> io::Result<bool> {
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame::<_, ClientFrame>(reader))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no hello within the timeout"))??;
    let reply = match hello {
        None => return Ok(false),
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
    state: Arc<ServerState>,
    outbox: Outbox,
    subscription: Option<JoinHandle<()>>,
}

impl Session {
    async fn read_frames(&mut self, reader: &mut ReadHalf<BoxStream>) {
        loop {
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
            if !self.handle_frame(&body).await {
                return;
            }
        }
    }

    /// Acts on one frame. Returns false when the client broke the protocol and
    /// the connection should close.
    async fn handle_frame(&mut self, body: &[u8]) -> bool {
        match decode_body::<ClientFrame>(body) {
            Ok(ClientFrame::Request { id, request }) => {
                let state = Arc::clone(&self.state);
                let outbox = self.outbox.clone();
                tokio::spawn(async move {
                    let outcome = rpc::dispatch(&state, request).await;
                    let _ = outbox.send(ServerFrame::Response { id, outcome }).await;
                });
                true
            }
            Ok(ClientFrame::Subscribe { after }) => {
                self.subscribe(after);
                true
            }
            Ok(ClientFrame::Hello { .. }) => false,
            Err(err) => self.answer_undecodable(body, &err.to_string()).await,
        }
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
    fn subscribe(&mut self, after: Option<u64>) {
        if let Some(previous) = self.subscription.take() {
            previous.abort();
        }
        let state = Arc::clone(&self.state);
        let outbox = self.outbox.clone();
        let cursor = after.unwrap_or(0);
        self.subscription = Some(tokio::spawn(forward_events(state, outbox, cursor)));
    }

    fn close(mut self) {
        if let Some(subscription) = self.subscription.take() {
            subscription.abort();
        }
    }
}

/// Sends every event after `cursor`, then each new one as it is published.
async fn forward_events(state: Arc<ServerState>, outbox: Outbox, mut cursor: u64) {
    let mut published = state.events.watch();
    loop {
        // Mark the current value seen before reading the log, so an event
        // published between the read and the wait still wakes this loop.
        published.borrow_and_update();
        for event in state.events.after(cursor) {
            cursor = event.seq;
            if outbox.send(ServerFrame::Event(event)).await.is_err() {
                return;
            }
        }
        if published.changed().await.is_err() {
            return;
        }
    }
}
