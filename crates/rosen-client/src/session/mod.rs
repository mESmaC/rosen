//! [`Session`]: the stable entry point Velocity links (SPEC.md D7, §9).
//!
//! A session is one agent's connection to its machine's daemon. It fixes the
//! agent identity once, as Velocity fixes the nick at launch, and fills it in
//! as the owner, author, resolver or submitter of every request that records
//! one. So no call site can act under the wrong name.
//!
//! Every typed request is still reachable through [`Session::client`]. The
//! session adds the operations that need the identity or span several
//! requests, so Velocity's whole agent flow runs through it alone (§5):
//! workspaces in place of `git worktree`, the AIRC channel in place of local
//! AIRC files, and review, receipts and landing in place of a GitHub PR.

mod channel;
mod landing;
mod watch;
mod workspace;

pub use channel::{ChannelMembership, WorkClaim};
pub use landing::MergeCheckout;
pub use watch::WorkspaceWatch;
pub use workspace::{CreatedWorkspace, WorkspaceSpec};

use crate::{identity, Client, Error};
use rosen_core::{AgentId, ChangeState, ObjectId};
use rosen_protocol::{
    ChangeFilter, ChangeRecord, ClaimOutcome, ClaimRequest, CommentRequest, CommentTarget,
    Endpoint, Event, NewChange, ReceiptRecord, ReleaseOutcome, ResolveRequest, ReviewThread,
    ShareOutcome, SubmitReceipt, WaitOutcome,
};
use std::sync::Arc;
use tokio::sync::mpsc;

/// One agent's connection to the machine's daemon. Cheap to clone: clones
/// share the connection.
#[derive(Clone)]
pub struct Session {
    client: Arc<Client>,
    endpoint: Endpoint,
    agent: AgentId,
}

/// How [`SessionBuilder::open`] finds the daemon and names the agent.
#[derive(Debug, Clone)]
pub struct SessionBuilder {
    endpoint: Option<Endpoint>,
    agent: Option<AgentId>,
    start_daemon: bool,
}

impl Default for SessionBuilder {
    fn default() -> Self {
        Self {
            endpoint: None,
            agent: None,
            start_daemon: true,
        }
    }
}

impl SessionBuilder {
    /// The daemon's endpoint. Defaults to [`Endpoint::resolve`]:
    /// `ROSEN_ENDPOINT`, else the per-user default.
    pub fn endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// The agent this session acts as. Defaults to
    /// [`identity::resolve_agent`] with nothing explicit.
    pub fn agent(mut self, agent: AgentId) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Whether to launch the daemon when none is running (default true).
    /// With false, a missing daemon is [`Error::NotRunning`].
    pub fn start_daemon(mut self, start: bool) -> Self {
        self.start_daemon = start;
        self
    }

    /// Resolves the agent before connecting, so a missing identity costs no
    /// daemon start.
    pub async fn open(self) -> Result<Session, Error> {
        let agent = match self.agent {
            Some(agent) => agent,
            None => identity::resolve_agent(None)?,
        };
        let endpoint = self.endpoint.unwrap_or_else(Endpoint::resolve);
        let client = if self.start_daemon {
            crate::connect_or_start(&endpoint).await?
        } else {
            Client::connect(&endpoint).await?
        };
        Ok(Session::from_client(Arc::new(client), endpoint, agent))
    }
}

impl Session {
    pub fn builder() -> SessionBuilder {
        SessionBuilder::default()
    }

    /// Opens a session for `agent` on the default endpoint, starting the
    /// daemon if needed.
    pub async fn open(agent: AgentId) -> Result<Session, Error> {
        Session::builder().agent(agent).open().await
    }

    /// Wraps a connection made elsewhere. `endpoint` must be the one it was
    /// made on: watchers reconnect to it after the daemon restarts.
    pub fn from_client(client: Arc<Client>, endpoint: Endpoint, agent: AgentId) -> Session {
        Session {
            client,
            endpoint,
            agent,
        }
    }

    /// Another agent's session over this one's connection, as a host serving
    /// several agents from one process does.
    pub fn as_agent(&self, agent: AgentId) -> Session {
        Session::from_client(Arc::clone(&self.client), self.endpoint.clone(), agent)
    }

    /// The same agent over a connection of its own. A connection carries one
    /// event stream, so a host that reads [`Session::events`] while also
    /// calling [`Session::wait_until_free`] needs two.
    pub async fn new_connection(&self) -> Result<Session, Error> {
        let client = Client::connect(&self.endpoint).await?;
        Ok(Session::from_client(
            Arc::new(client),
            self.endpoint.clone(),
            self.agent.clone(),
        ))
    }

    /// The full typed request API.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn shared_client(&self) -> Arc<Client> {
        Arc::clone(&self.client)
    }

    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Creates a change owned by this agent, on the base or stacked on `on`
    /// (a change ID or unique prefix). Use [`Session::create_workspace`] to
    /// make its workspace in the same step.
    pub async fn new_change(&self, on: Option<&str>) -> Result<ChangeRecord, Error> {
        let request = NewChange {
            owner: self.agent.clone(),
            on: on.map(str::to_owned),
        };
        self.client.change_new(request).await
    }

    /// This agent's changes, optionally only those in `state`.
    pub async fn my_changes(&self, state: Option<ChangeState>) -> Result<Vec<ChangeRecord>, Error> {
        let filter = ChangeFilter {
            owner: Some(self.agent.clone()),
            state,
        };
        self.client.change_list(filter).await
    }

    /// Abandons a change. Its workspaces on this machine get a final
    /// snapshot first and are then deleted under the deletion guard (R4).
    pub async fn abandon(&self, change: &str) -> Result<ChangeRecord, Error> {
        self.client.change_abandon(change).await
    }
}

/// Path claims (R6). `change` is a change ID or unique prefix; refusals are
/// outcomes naming the holder, not errors.
impl Session {
    pub async fn claim(&self, change: &str, paths: &[String]) -> Result<ClaimOutcome, Error> {
        self.client.claim_paths(claim_request(change, paths)).await
    }

    pub async fn share(&self, change: &str, paths: &[String]) -> Result<ShareOutcome, Error> {
        self.client.share_paths(claim_request(change, paths)).await
    }

    /// Records a wait and returns at once.
    pub async fn wait(&self, change: &str, paths: &[String]) -> Result<WaitOutcome, Error> {
        self.client.wait_paths(claim_request(change, paths)).await
    }

    /// Records a wait and blocks until every waited path is released. It
    /// reads events on this session's connection, replacing any stream from
    /// [`Session::events`] there; see [`Session::new_connection`].
    pub async fn wait_until_free(
        &self,
        change: &str,
        paths: &[String],
    ) -> Result<WaitOutcome, Error> {
        self.client
            .wait_until_free(claim_request(change, paths))
            .await
    }

    /// Drops the change's records on `paths`, or on every path when empty.
    pub async fn release(&self, change: &str, paths: &[String]) -> Result<ReleaseOutcome, Error> {
        self.client
            .release_paths(claim_request(change, paths))
            .await
    }
}

fn claim_request(change: &str, paths: &[String]) -> ClaimRequest {
    ClaimRequest {
        change: change.to_owned(),
        paths: paths.to_vec(),
    }
}

/// Review (R7) and receipts (R8), recorded under this agent.
impl Session {
    /// Comments on a line of the change's head, or replies in a thread.
    /// `head` is the tree the agent read; the server refuses the comment once
    /// the change has another head, so line numbers never drift.
    pub async fn comment(
        &self,
        change: &str,
        target: CommentTarget,
        body: impl Into<String>,
        head: Option<ObjectId>,
    ) -> Result<ReviewThread, Error> {
        let request = CommentRequest {
            change: change.to_owned(),
            author: self.agent.clone(),
            body: body.into(),
            head,
            target,
        };
        self.client.review_comment(request).await
    }

    pub async fn resolve_thread(&self, thread: u64) -> Result<ReviewThread, Error> {
        let request = ResolveRequest {
            thread,
            by: self.agent.clone(),
        };
        self.client.review_resolve(request).await
    }

    /// Records that `check_set` passed on the change's merge result of
    /// `head` onto base commit `base`, as [`Client::merge_result`] gave it.
    pub async fn submit_receipt(
        &self,
        change: &str,
        head: ObjectId,
        base: ObjectId,
        check_set: &str,
    ) -> Result<ReceiptRecord, Error> {
        let request = SubmitReceipt {
            change: change.to_owned(),
            head,
            base,
            check_set: check_set.to_owned(),
            submitter: self.agent.clone(),
            signature: None,
        };
        self.client.submit_receipt(request).await
    }
}

/// Events (R9) for this agent.
impl Session {
    /// The events that concern this agent, plus broadcasts. With `after:
    /// None` the stream resumes after the agent's acknowledged cursor, so it
    /// survives a reconnect or a server restart. Starting another stream on
    /// the same connection closes this one; use a separate session per
    /// concurrent stream.
    pub async fn events(&self, after: Option<u64>) -> Result<mpsc::Receiver<Event>, Error> {
        self.client.subscribe_as(self.agent.clone(), after).await
    }

    /// Acknowledges every event up to `seq` once handled. Returns the stored
    /// cursor, which never moves backwards.
    pub async fn ack_events(&self, seq: u64) -> Result<u64, Error> {
        self.client.ack_events(self.agent.clone(), seq).await
    }
}
