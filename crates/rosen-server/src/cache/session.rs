//! One local client of a cache daemon (SPEC D8).
//!
//! Liveness, status, shutdown and object reads are answered here, from the
//! machine's store. Every other frame is forwarded to the server as it
//! arrived, over this connection's own upstream link and in the order the
//! client sent it, so the server sees the same sequence a direct client
//! would. Requests this build does not know are forwarded too.

use super::fill;
use super::partial::CompleteView;
use super::read;
use super::upstream::{self, error_response, Relay, UpstreamLink};
use super::CacheState;
use crate::connection::handshake;
use crate::load_limits::{Charged, TransferLimits};
use crate::network::UpstreamConfig;
use rosen_core::{ChangeId, Receipt};
use rosen_protocol::frame::decode_body;
use rosen_protocol::{
    BoxStream, ClientFrame, ErrorBody, ErrorCode, ObjectUpload, Outcome, Request, Response,
    ServerFrame, SubmitReceipt,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::ReadHalf;
use tokio::sync::{mpsc, watch, Mutex, OwnedSemaphorePermit};

/// Frames queued for the local client, as the server bounds its own.
const OUTBOX_CAPACITY: usize = 16;

/// Serves one local client until it disconnects, the daemon stops, or its
/// event stream's upstream link is lost.
pub async fn serve(stream: BoxStream, state: Arc<CacheState>) {
    let _open = state.track_connection();
    let (mut reader, mut writer) = tokio::io::split(stream);
    // The local transport is already limited to the OS user.
    match handshake(&mut reader, &mut writer, &state.transfers, |_| async {
        true
    })
    .await
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => {
            eprintln!("rosen-server: handshake failed: {err}");
            return;
        }
    }
    let (outbox, inbox) = mpsc::channel(OUTBOX_CAPACITY);
    let mut writer_task = tokio::spawn(upstream::write_bodies(writer, inbox));
    let session = Session {
        links: Arc::new(LinkSlot::new(
            state.upstream.clone(),
            outbox.clone(),
            state.replies.clone(),
        )),
        state,
        outbox,
    };
    tokio::select! {
        _ = session.read_frames(&mut reader) => {}
        _ = &mut writer_task => return,
    }
    drop(session);
    // The writer ends once every queued frame is written and every sender
    // (in-flight requests and the upstream link's router) is gone.
    let _ = writer_task.await;
}

/// The connection's upstream link, made on first use and remade after a
/// loss, unless the client had subscribed to events over the lost one.
pub struct LinkSlot {
    config: UpstreamConfig,
    relay: Relay,
    replies: Arc<TransferLimits>,
    lost: Arc<watch::Sender<bool>>,
    slot: Mutex<Option<Arc<UpstreamLink>>>,
}

impl LinkSlot {
    fn new(config: UpstreamConfig, relay: Relay, replies: Arc<TransferLimits>) -> Self {
        Self {
            config,
            relay,
            replies,
            lost: Arc::new(watch::Sender::new(false)),
            slot: Mutex::new(None),
        }
    }

    /// The open link, connecting first if there is none.
    pub async fn get(&self) -> Result<Arc<UpstreamLink>, ErrorBody> {
        let mut slot = self.slot.lock().await;
        if let Some(link) = slot.as_ref().filter(|link| link.is_open()) {
            return Ok(Arc::clone(link));
        }
        if *self.lost.borrow() {
            return Err(upstream::unavailable());
        }
        let relay = self.relay.clone();
        match UpstreamLink::connect(
            &self.config,
            relay,
            Arc::clone(&self.lost),
            self.replies.clone(),
        )
        .await
        {
            Ok(link) => Ok(Arc::clone(slot.insert(Arc::new(link)))),
            Err(err) => Err(ErrorBody::new(
                ErrorCode::Unavailable,
                format!(
                    "could not reach the server at {}: {err}",
                    self.config.address
                ),
            )),
        }
    }

    /// Closes the client's connection, so it notices its stream is gone.
    fn mark_lost(&self) {
        self.lost.send_replace(true);
    }
}

struct Session {
    state: Arc<CacheState>,
    outbox: Relay,
    links: Arc<LinkSlot>,
}

impl Session {
    async fn read_frames(&self, reader: &mut ReadHalf<BoxStream>) {
        let mut lost = self.links.lost.subscribe();
        loop {
            let read = tokio::select! {
                read = self.state.transfers.read_body(reader) => read,
                _ = self.state.shutdown_requested() => return,
                _ = lost.wait_for(|lost| *lost) => return,
            };
            match read {
                Ok(Some(body)) => {
                    if !self.handle(body).await {
                        return;
                    }
                }
                Ok(None) => return,
                Err(err) => {
                    eprintln!("rosen-server: dropping client after read error: {err}");
                    return;
                }
            }
        }
    }

    /// Acts on one frame. Returns false when the connection should close.
    async fn handle(&self, body: Charged<Vec<u8>>) -> bool {
        match decode_body::<ClientFrame>(&body.value) {
            Ok(ClientFrame::Request { id, request }) => {
                self.request(id, request, body).await;
                true
            }
            Ok(ClientFrame::Subscribe { .. }) => {
                let subscribed = match self.links.get().await {
                    Ok(link) => link.subscribe(body).await,
                    Err(_) => false,
                };
                if !subscribed {
                    // A subscribe has no answer, so closing is the only signal.
                    self.links.mark_lost();
                }
                true
            }
            Ok(ClientFrame::Hello { .. }) => false,
            // A request from a newer client goes to the server, which knows
            // whether it can answer it.
            Err(_) => match decode_body::<Value>(&body.value) {
                Ok(frame) if frame["type"] == "request" => match frame["id"].as_u64() {
                    Some(id) => {
                        self.forward(
                            id,
                            Charged {
                                value: frame,
                                permit: body.permit,
                            },
                        )
                        .await;
                        true
                    }
                    None => false,
                },
                _ => false,
            },
        }
    }

    async fn request(&self, id: u64, request: Request, body: Charged<Vec<u8>>) {
        match request {
            Request::PoolSize { size } => {
                self.reply(id, self.state.pool.set_size(size).await).await;
            }
            Request::PoolTake { tree, dest } => {
                self.spawn(id, body.permit, move |state, _| async move {
                    match state.pool.take(tree, dest).await {
                        Outcome::Ok(response) => Ok(response),
                        Outcome::Error(error) => Err(error),
                    }
                })
                .await
            }
            Request::Ping => self.reply(id, Outcome::Ok(Response::Pong)).await,
            Request::Status => {
                let status = Response::Status(self.state.status());
                self.reply(id, Outcome::Ok(status)).await;
            }
            Request::Shutdown => {
                // Stops this machine's daemon, never the project's server.
                self.state.request_shutdown();
                self.reply(id, Outcome::Ok(Response::ShuttingDown)).await;
            }
            Request::GetObjects(query) => {
                self.spawn(id, body.permit, move |state, links| async move {
                    fill::get_objects(&state.store, &state.partial, &links, query)
                        .await
                        .map(Response::Objects)
                })
                .await
            }
            Request::GetObjectChunk(query) => {
                self.spawn(id, body.permit, move |state, links| async move {
                    read::get_chunk(&state, &links, query)
                        .await
                        .map(Response::ObjectChunk)
                })
                .await
            }
            Request::EnsureObjects(ensure) => {
                self.spawn(id, body.permit, move |state, links| async move {
                    fill::ensure(
                        &state.store,
                        &state.partial,
                        &links,
                        ensure.root,
                        &ensure.sparse,
                    )
                    .await
                    .map(|fetched| Response::ObjectsLocal(rosen_protocol::ObjectsLocal { fetched }))
                })
                .await
            }
            Request::PutObjects(upload) => match self.keep_uploaded(upload, body).await {
                Ok(body) => self.forward_body(id, body).await,
                Err(error) => self.reply(id, Outcome::Error(error)).await,
            },
            Request::ReceiptSubmit(submit) => self.forward_signed(id, submit, body.permit).await,
            _ => self.forward_body(id, body).await,
        }
    }

    /// Signs a receipt with this machine's key and forwards it (SPEC.md §2,
    /// R14). Done in the read loop, so the receipt keeps its place among the
    /// client's frames; a receipt is rare enough that the wait is cheap.
    async fn forward_signed(
        &self,
        id: u64,
        mut submit: SubmitReceipt,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        if let Err(error) = self.sign_receipt(&mut submit).await {
            let _ = self.outbox.send(error_response(id, error).into()).await;
            return;
        }
        let request = Request::ReceiptSubmit(submit);
        let frame = ClientFrame::Request { id, request };
        match serde_json::to_value(&frame) {
            Ok(value) => self.forward(id, Charged { value, permit }).await,
            Err(err) => {
                let error = ErrorBody::new(ErrorCode::Internal, err.to_string());
                let _ = self.outbox.send(error_response(id, error).into()).await;
            }
        }
    }

    /// The signature covers the full change ID, so a prefix the client gave
    /// is resolved on the server first.
    async fn sign_receipt(&self, submit: &mut SubmitReceipt) -> Result<(), ErrorBody> {
        let change = match submit.change.parse::<ChangeId>() {
            Ok(change) => change,
            Err(_) => self.resolve_change(&submit.change).await?,
        };
        let receipt = Receipt {
            change,
            head: submit.head,
            base: submit.base,
            check_set: submit.check_set.clone(),
            submitter: submit.submitter.clone(),
        };
        // Loading may create the key file, so it runs off the async workers.
        let state = Arc::clone(&self.state);
        let loaded = tokio::task::spawn_blocking(move || state.signing_key()).await;
        let key = match loaded {
            Ok(Ok(key)) => key,
            Ok(Err(err)) => {
                let why = format!("could not load this machine's receipt signing key: {err}");
                return Err(ErrorBody::new(ErrorCode::Internal, why));
            }
            Err(join) => return Err(ErrorBody::new(ErrorCode::Internal, join.to_string())),
        };
        submit.change = change.to_string();
        submit.signature = Some(key.sign(&receipt.signing_payload()));
        Ok(())
    }

    async fn resolve_change(&self, change: &str) -> Result<ChangeId, ErrorBody> {
        let link = self.links.get().await?;
        let show = Request::ChangeShow {
            change: change.to_owned(),
        };
        match link.request(show).await {
            Outcome::Ok(Response::Change(record)) => Ok(record.change.id),
            Outcome::Ok(_) => Err(ErrorBody::new(
                ErrorCode::Internal,
                "the server answered a change lookup with something else",
            )),
            Outcome::Error(error) => Err(error),
        }
    }

    /// Runs a locally answered request on its own task, so a long fill never
    /// delays the client's other requests.
    async fn spawn<F, Fut>(&self, id: u64, input: Option<OwnedSemaphorePermit>, work: F)
    where
        F: FnOnce(Arc<CacheState>, Arc<LinkSlot>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Response, ErrorBody>> + Send + 'static,
    {
        let permit = self
            .state
            .transfers
            .large_requests
            .clone()
            .acquire_owned()
            .await
            .expect("transfer limits stay open");
        let task = work(Arc::clone(&self.state), Arc::clone(&self.links));
        let outbox = self.outbox.clone();
        tokio::spawn(async move {
            let outcome = match task.await {
                Ok(response) => Outcome::Ok(response),
                Err(error) => Outcome::Error(error),
            };
            drop(input);
            let _ = outbox
                .send(Charged {
                    value: encode_response(id, outcome),
                    permit: Some(permit),
                })
                .await;
        });
    }

    /// Uploaded objects are kept on this machine too, so its workspaces read
    /// them without a round trip. The server still checks and stores them;
    /// the same completeness rule keeps an incomplete tree out of the cache.
    async fn keep_uploaded(
        &self,
        upload: ObjectUpload,
        body: Charged<Vec<u8>>,
    ) -> Result<Charged<Vec<u8>>, ErrorBody> {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || {
            // A tree naming a partial one is not complete either.
            let store = CompleteView {
                store: &state.store,
                partial: &state.partial,
            };
            let _ = crate::snapshots::store_objects(&store, &upload);
            // The work can outlive a disconnect. Keep the reservation until
            // it releases the upload bytes, even if the read loop is gone.
            body
        })
        .await
        .map_err(|error| ErrorBody::new(ErrorCode::Internal, error.to_string()))
    }

    async fn forward_body(&self, id: u64, body: Charged<Vec<u8>>) {
        let Charged { value, permit } = body;
        let frame = decode_body::<Value>(&value);
        drop(value);
        match frame {
            Ok(value) => self.forward(id, Charged { value, permit }).await,
            Err(err) => {
                let error = ErrorBody::new(ErrorCode::BadFrame, err.to_string());
                let _ = self.outbox.send(error_response(id, error).into()).await;
            }
        }
    }

    /// Forwards in the read loop, not on a task, so frames reach the server
    /// in the order the client sent them.
    async fn forward(&self, id: u64, frame: Charged<Value>) {
        let error = match self.links.get().await {
            Ok(link) if link.forward(id, frame).await => return,
            Ok(_) => upstream::unavailable(),
            Err(error) => error,
        };
        let _ = self.outbox.send(error_response(id, error).into()).await;
    }

    async fn reply(&self, id: u64, outcome: Outcome) {
        let _ = self.outbox.send(encode_response(id, outcome).into()).await;
    }
}

fn encode_response(id: u64, outcome: Outcome) -> Vec<u8> {
    let frame = ServerFrame::Response { id, outcome };
    serde_json::to_vec(&frame).expect("a response always serialises")
}
