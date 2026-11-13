//! One simulated agent: it creates changes with workspaces (some stacked),
//! edits them, claims paths, gets a receipt and an approval, and lands,
//! through `rosen-client` alone, as Velocity's agents will (§5, §9).
//!
//! Everything an agent does goes to its machine's journal first. When the
//! `agents` process is killed and started again, each agent carries on
//! with the changes it had begun instead of making new ones, as a real
//! agent recovering from a crash would.

use crate::claims::HotClaims;
use crate::machine::{Progress, Shared};
use crate::report::{Began, ChangeOutcome, LandSample, ReceiptSample, Record, StartSample};
use anyhow::{anyhow, bail, Result};
use rosen_client::protocol::{
    ApproveRequest, ClaimOutcome, ErrorCode, LandStatus, LogRequest, MergeResultAnswer, ReadAnswer,
    ReadRequest,
};
use rosen_client::{AgentId, ChangeId, Error, ObjectId, Session, WorkspaceSpec};
use rosen_core::receipt::PRE_LAND_CHECK_SET;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Land requests one change may make before the agent gives up on it.
/// Each needs a fresh receipt because another change in its unit landed
/// first, so this bounds contention, not correctness.
const MAX_LAND_ROUNDS: u32 = 200;

/// Reconnect attempts after the daemon goes away, a few minutes in all,
/// longer than any outage a fault plan makes.
const MAX_RECONNECTS: u32 = 120;

/// Longest a request may take. A silent partition holds a request without
/// closing it, so only this notices; it is far longer than any planned
/// partition or loaded answer, so it fires only on a request a fault left
/// stranded. Retrying sooner would only add load to a busy server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// Longest a workspace creation may take, including its wait for a
/// materialisation slot under the daemon's load limits (R3).
const CREATE_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Agent {
    pub index: usize,
    pub name: AgentId,
    pub session: Session,
    pub shared: Arc<Shared>,
    /// What this agent's earlier processes did, from the journal.
    pub progress: Progress,
}

/// A change this agent made and is carrying to the base.
struct Work {
    n: usize,
    change: ChangeId,
    root: PathBuf,
    stacked_on: Option<ChangeId>,
    expected: Vec<(String, String)>,
    /// Found already on the base when an earlier process's land of it was
    /// never recorded.
    landed: Option<ObjectId>,
}

impl From<&Began> for Work {
    fn from(began: &Began) -> Self {
        Self {
            n: began.n,
            change: began.change,
            root: began.root.clone(),
            stacked_on: began.stacked_on,
            expected: began.expected.clone(),
            landed: None,
        }
    }
}

/// A request that failed for good, or a sign that it might pass if sent
/// again on a new connection.
enum Failure {
    Transient(String),
    Fatal(anyhow::Error),
}

/// Sends one request with a time limit and sorts its failure.
async fn attempt<T>(
    call: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<T, Failure> {
    match tokio::time::timeout(REQUEST_TIMEOUT, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) if is_transient(&err) => Err(Failure::Transient(err.to_string())),
        Ok(Err(err)) => Err(Failure::Fatal(err.into())),
        Err(_) => Err(Failure::Transient(format!(
            "no answer in {REQUEST_TIMEOUT:?}"
        ))),
    }
}

/// Retries a request across a lost connection, reconnecting between
/// tries. Only for requests that are safe to repeat: the server answers a
/// repeated land with `AlreadyLanded`, never a second land.
macro_rules! retrying {
    ($agent:expr, $what:expr, |$session:ident| $call:expr) => {{
        let mut tries = 0;
        loop {
            let $session = $agent.session.clone();
            match attempt($call).await {
                Ok(value) => break Ok(value),
                Err(Failure::Transient(why)) if tries < MAX_RECONNECTS => {
                    tries += 1;
                    $agent.note(format!("{}: {why}; reconnecting", $what));
                    $agent.reconnect().await;
                }
                Err(Failure::Transient(why)) => break Err(anyhow!("{}: {why}", $what)),
                Err(Failure::Fatal(err)) => break Err(err.context($what)),
            }
        }
    }};
}

impl Agent {
    pub async fn run(mut self) {
        let plan = self.shared.plan.clone();
        let mut n = 0;
        while n < plan.lands_per_agent {
            let mut batch = vec![n];
            while n + batch.len() < plan.lands_per_agent && plan.is_stacked(n + batch.len()) {
                batch.push(n + batch.len());
            }
            n += batch.len();
            if !batch.iter().all(|n| self.progress.finished.contains_key(n)) {
                self.run_batch(&batch).await;
            }
        }
    }

    /// Makes one change per number, each stacked on the one before, then
    /// lands them bottom first (R8 step 3). Changes an earlier process of
    /// this agent began are carried on, and finished ones are skipped.
    async fn run_batch(&mut self, numbers: &[usize]) {
        let stack = self.start_batch(numbers).await;
        let mut blocked = numbers
            .iter()
            .any(|n| self.progress.finished.get(n) == Some(&false))
            .then(|| "a change below it did not land".to_owned());
        for work in &stack {
            let result = match (&blocked, work.landed) {
                (_, Some(commit)) => Ok(commit),
                (Some(why), None) => Err(anyhow!("parent did not land: {why}")),
                (None, None) => self.land(work).await,
            };
            if let Err(err) = &result {
                blocked.get_or_insert_with(|| format!("{err:#}"));
            }
            self.finish(work, result).await;
        }
    }

    /// The batch's unfinished changes, begun now or by an earlier process.
    async fn start_batch(&mut self, numbers: &[usize]) -> Vec<Work> {
        let mut stack: Vec<Work> = Vec::new();
        let mut on = None;
        let landing_began = numbers
            .iter()
            .any(|n| self.progress.finished.contains_key(n));
        for &n in numbers {
            let began = self.progress.began.get(&n).cloned();
            if self.progress.finished.contains_key(&n) {
                on = began.map(|b| b.change);
                continue;
            }
            let started = match began {
                Some(began) => self.resume_change(&began).await,
                // Its stack started landing, so it is too late to add it.
                None if landing_began => Err(anyhow!("its stack is already landing")),
                None => self.start_change(n, on).await,
            };
            match started {
                Ok(work) => {
                    on = Some(work.change);
                    stack.push(work);
                }
                Err(err) => {
                    self.note(format!("change {n} not started: {err:#}"));
                    break;
                }
            }
        }
        stack
    }

    async fn start_change(&mut self, n: usize, on: Option<ChangeId>) -> Result<Work> {
        let (change, root) = self.create_workspace(n, on).await?;
        let plan = self.shared.plan.clone();
        let expected = crate::edit::write_edits(&plan, self.index, n, &root)?;
        let began = Began {
            index: self.index,
            n,
            change,
            root,
            stacked_on: on,
            expected,
        };
        self.shared.record(Record::Began(began.clone()));
        self.claim(n, change, true).await?;
        self.snapshot(&began).await?;
        Ok(Work::from(&began))
    }

    /// Creates the change and its workspace, measuring the start (R3). A
    /// try a fault cuts short is retried as a new change in a new
    /// directory, and the change it left behind is abandoned, so outages
    /// never pile empty changes up on the server.
    async fn create_workspace(
        &mut self,
        n: usize,
        on: Option<ChangeId>,
    ) -> Result<(ChangeId, PathBuf)> {
        let base = self
            .shared
            .work_dir
            .join(self.shared.plan.agent_name(self.index));
        for tries in 0..=MAX_RECONNECTS {
            let root = fresh_root(&base, n, tries);
            let mut spec = WorkspaceSpec::new(&root);
            if let Some(parent) = on {
                spec = spec.on(parent.to_string());
            }
            let started = Instant::now();
            let created = tokio::time::timeout(CREATE_TIMEOUT, self.session.create_workspace(spec));
            let (err, left_behind) = match created.await {
                Ok(Ok(created)) => {
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    let method = created.report.method.as_str().to_owned();
                    self.shared
                        .record(Record::Start(StartSample { ms, method }));
                    return Ok((created.change.change.id, root));
                }
                // A creation that made its change but not its directory is
                // retried in a fresh directory whatever the cause: an earlier
                // try may still be writing the old one, or a sibling's
                // deletion may have raced it (#161).
                Ok(Err(err))
                    if !is_transient(&err) && !matches!(err, Error::WorkspaceNotCreated { .. }) =>
                {
                    return Err(anyhow::Error::new(err).context("create workspace"));
                }
                Ok(Err(err)) => {
                    let left_behind = match &err {
                        Error::WorkspaceNotCreated { change, .. } => Some(*change),
                        _ => None,
                    };
                    (err.to_string(), left_behind)
                }
                Err(_) => (format!("no answer in {CREATE_TIMEOUT:?}"), None),
            };
            if tries == MAX_RECONNECTS {
                bail!("create workspace: {err}");
            }
            self.note(format!("create workspace: {err}; reconnecting"));
            self.reconnect().await;
            if let Some(change) = left_behind {
                self.abandon_left_behind(change).await;
            }
        }
        unreachable!("the loop returns on its last try")
    }

    /// Abandons the change a cut-short workspace creation left behind.
    async fn abandon_left_behind(&mut self, change: ChangeId) {
        let id = change.to_string();
        if let Err(err) = retrying!(self, "abandon", |s| s.abandon(&id)) {
            self.note(format!("{err:#}"));
        }
    }

    /// Carries on a change an earlier process began: its edits and
    /// snapshot may not have happened, and its claims may be missing.
    async fn resume_change(&mut self, began: &Began) -> Result<Work> {
        let id = began.change.to_string();
        let record = retrying!(self, "show change", |s| s.client().change_show(&id))?;
        let open = matches!(
            record.change.state,
            rosen_core::ChangeState::Open | rosen_core::ChangeState::Ready
        );
        let mut work = Work::from(began);
        if open {
            // A revert can reopen a change whose land this agent never
            // recorded (R12); landing it again would apply it twice.
            work.landed = self.landed_on_base(began).await?;
        }
        if work.landed.is_some() {
            return Ok(work);
        }
        if open && !self.head_has_edits(began).await? {
            let plan = self.shared.plan.clone();
            crate::edit::write_edits(&plan, self.index, began.n, &began.root)?;
            self.claim(began.n, began.change, false).await?;
            self.snapshot(began).await?;
        } else if open {
            self.claim(began.n, began.change, false).await?;
        }
        Ok(work)
    }

    /// The commit that landed the change, found through the file only it
    /// adds, so the search reads one path's history.
    async fn landed_on_base(&mut self, began: &Began) -> Result<Option<ObjectId>> {
        let request = LogRequest {
            at: None,
            paths: vec![self.shared.plan.own_path(self.index, began.n)],
            limit: Some(20),
        };
        let log = retrying!(self, "path history", |s| s.client().log(request.clone()))?;
        let landed = log
            .entries
            .iter()
            .find(|entry| entry.change == Some(began.change) && entry.receipt.is_some());
        Ok(landed.map(|entry| entry.commit))
    }

    /// Whether the change's stored head already holds every expected line.
    async fn head_has_edits(&mut self, began: &Began) -> Result<bool> {
        for (path, line) in &began.expected {
            let request = ReadRequest {
                at: Some(began.change.to_string()),
                path: path.clone(),
            };
            let read = retrying!(self, "read head", |s| s.client().read_file(request.clone()));
            let holds = match read {
                Ok(ReadAnswer::File { data, .. }) => {
                    String::from_utf8_lossy(&data).lines().any(|l| l == line)
                }
                _ => false,
            };
            if !holds {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn snapshot(&mut self, began: &Began) -> Result<()> {
        let message = format!("{} change {}", self.name, began.n);
        let root = began.root.clone();
        retrying!(self, "snapshot", |s| s
            .snapshot(&root, Some(message.clone())))?;
        Ok(())
    }

    /// Claims the change's own new file, shares its unit's slots file, and
    /// on some changes races the unit's other agents for the hot path.
    async fn claim(&mut self, n: usize, change: ChangeId, race: bool) -> Result<()> {
        let plan = self.shared.plan.clone();
        let id = change.to_string();
        let own = vec![plan.own_path(self.index, n)];
        let claimed = retrying!(self, "claim", |s| s.claim(&id, &own))?;
        if !matches!(claimed, ClaimOutcome::Granted { .. }) {
            self.violation(format!("{id}: its own new path was refused: {claimed:?}"));
        }
        let unit = plan.unit_of(self.index);
        let slots = vec![crate::plan::Plan::slots_path(unit)];
        retrying!(self, "share", |s| s.share(&id, &slots))?;
        if race && plan.claims_hot(n) {
            let hot = crate::plan::Plan::hot_path(unit);
            let paths = vec![hot.clone()];
            match retrying!(self, "claim hot", |s| s.claim(&id, &paths))? {
                ClaimOutcome::Granted { .. } => self.hot_granted(change, &hot).await?,
                ClaimOutcome::Refused { .. } => {
                    self.shared.record(Record::HotClaim { granted: false });
                }
            }
        }
        Ok(())
    }

    async fn hot_granted(&mut self, change: ChangeId, hot: &str) -> Result<()> {
        self.shared.record(Record::HotClaim { granted: true });
        if let Some(clash) = self.shared.hot.granted(hot, self.index, change) {
            self.violation(clash);
        }
        let claims = retrying!(self, "claim list", |s| s
            .client()
            .claim_list(Default::default()))?;
        if let Some(clash) = HotClaims::server_clash(hot, &claims) {
            self.violation(clash);
        }
        Ok(())
    }

    /// Receipts the change's current merge result, approves its head and
    /// asks to land, repeating while another land made the receipt stale.
    async fn land(&mut self, work: &Work) -> Result<ObjectId> {
        let id = work.change.to_string();
        let mut approved: Option<ObjectId> = None;
        self.shared.hot.landing(work.change);
        for round in 0..MAX_LAND_ROUNDS {
            let merge = retrying!(self, "merge result", |s| s.client().merge_result(&id));
            let merge = match merge {
                Ok(merge) => merge,
                Err(err) => return self.landed_already(&id, err).await,
            };
            if merge.tree.is_none() {
                bail!("merge conflict in {:?}", merge.conflicts);
            }
            if !self
                .check_and_review(work.change, &merge, &mut approved)
                .await?
            {
                continue;
            }
            let started = Instant::now();
            let answer = match retrying!(self, "land", |s| s.client().land(&id)) {
                Ok(answer) => answer,
                Err(err) if is_contended(&err) => {
                    self.shared.record(Record::Refusal {
                        reason: "contended".into(),
                    });
                    tokio::time::sleep(contention_backoff(self.index, round)).await;
                    continue;
                }
                Err(err) => return Err(err),
            };
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            match answer.outcome {
                LandStatus::Landed { commit, attempts } => {
                    let change = work.change;
                    let rechecks = round;
                    let sample = LandSample {
                        change,
                        ms,
                        attempts,
                        rechecks,
                    };
                    self.shared.record(Record::Land(sample));
                    return Ok(commit);
                }
                LandStatus::AlreadyLanded { commit } => return Ok(commit),
                LandStatus::Refused {
                    reason, message, ..
                } => {
                    self.shared.record(Record::Refusal {
                        reason: reason.clone(),
                    });
                    match reason.as_str() {
                        "needs_recheck" | "receipt_stale" | "no_receipt" => {}
                        "unapproved" => approved = None,
                        _ => bail!("land refused: {reason}: {message}"),
                    }
                }
            }
        }
        bail!("still not landed after {MAX_LAND_ROUNDS} receipts")
    }

    /// A resumed agent whose earlier process landed the change before it
    /// could record so: the merge result is refused, but a land request is
    /// answered `AlreadyLanded` with the commit.
    async fn landed_already(&mut self, id: &str, err: anyhow::Error) -> Result<ObjectId> {
        match retrying!(self, "land", |s| s.client().land(id)) {
            Ok(answer) => match answer.outcome {
                LandStatus::AlreadyLanded { commit } => Ok(commit),
                _ => Err(err),
            },
            Err(_) => Err(err),
        }
    }

    /// Receipts the merge result and approves its head, unless it has
    /// approved that head already. `Ok(false)` means the server rebased the
    /// change after the merge result was read (R8 step 4), so the agent
    /// reads it again.
    async fn check_and_review(
        &mut self,
        change: ChangeId,
        merge: &MergeResultAnswer,
        approved: &mut Option<ObjectId>,
    ) -> Result<bool> {
        let id = change.to_string();
        let mut result = self.receipt(change, merge.head, merge.base).await;
        if result.is_ok() && *approved != Some(merge.head) {
            result = self.approve(&id, merge.head).await;
            if result.is_ok() {
                *approved = Some(merge.head);
            }
        }
        let Err(err) = result else {
            return Ok(true);
        };
        if !self.head_moved(&id, merge.head, &err).await {
            return Err(err);
        }
        let reason = "head_moved_before_land".to_owned();
        self.shared.record(Record::Refusal { reason });
        Ok(false)
    }

    /// The checker's part (D9): it would build and lint the merge result;
    /// the harness measures Rosen, not a build, so it receipts at once.
    async fn receipt(&mut self, change: ChangeId, head: ObjectId, base: ObjectId) -> Result<()> {
        let id = change.to_string();
        let checker = self.session.as_agent(self.shared.checker.clone());
        retrying!(self, "receipt", |_s| checker.submit_receipt(
            &id,
            head,
            base,
            PRE_LAND_CHECK_SET
        ))?;
        let sample = ReceiptSample { change, head, base };
        self.shared.record(Record::Receipt(sample));
        Ok(())
    }

    /// Whether a receipt or approval was refused only because the change
    /// has another head than the one the agent read.
    async fn head_moved(&mut self, id: &str, receipted: ObjectId, err: &anyhow::Error) -> bool {
        let refused = matches!(
            err.downcast_ref::<Error>(),
            Some(Error::Server(body)) if body.code == ErrorCode::Refused
        );
        if !refused {
            return false;
        }
        match retrying!(self, "show change", |s| s.client().change_show(id)) {
            Ok(record) => record.change.head != receipted,
            Err(_) => false,
        }
    }

    async fn approve(&mut self, id: &str, head: ObjectId) -> Result<()> {
        let request = ApproveRequest {
            change: id.to_owned(),
            reviewer: self.shared.reviewer.clone(),
            head,
        };
        retrying!(self, "approve", |s| s
            .client()
            .review_approve(request.clone()))?;
        Ok(())
    }

    /// Records the change's outcome and abandons it if it did not land.
    /// The daemon deletes the workspace itself once the change lands or is
    /// abandoned (R4); the machine run checks that it did.
    async fn finish(&mut self, work: &Work, result: Result<ObjectId>) {
        let id = work.change.to_string();
        let failure = result.as_ref().err().map(|err| format!("{err:#}"));
        if let Some(why) = &failure {
            self.note(format!("{id} (change {}) did not land: {why}", work.n));
            if let Err(err) = retrying!(self, "abandon", |s| s.abandon(&id)) {
                self.note(format!("{err:#}"));
            }
        }
        self.shared.hot.released(work.change);
        let outcome = ChangeOutcome {
            change: work.change,
            agent: self.name.to_string(),
            index: self.index,
            n: work.n,
            stacked_on: work.stacked_on,
            expected: work.expected.clone(),
            workspace: work.root.clone(),
            landed: result.ok(),
            failure,
        };
        self.shared.record(Record::Outcome(outcome));
    }

    fn note(&self, message: String) {
        let line = format!("{}: {message}", self.name);
        self.shared.record(Record::Error { line });
    }

    fn violation(&self, message: String) {
        let line = format!("{}: {message}", self.name);
        self.shared.record(Record::Violation { line });
    }

    /// Waits for the daemon to come back and replaces the connection. The
    /// harness never starts a daemon itself: on a machine that is not the
    /// server, which daemon runs and where is the machine's business.
    async fn reconnect(&mut self) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let opened = Session::builder()
            .endpoint(self.shared.endpoint.clone())
            .agent(self.name.clone())
            .start_daemon(false)
            .open();
        if let Ok(Ok(session)) = tokio::time::timeout(REQUEST_TIMEOUT, opened).await {
            self.session = session;
        }
    }
}

/// The server lost its compare-and-swap race too many times in a row and
/// gave the land back (R8 step 2). Nothing was applied, so the agent asks
/// again after a pause, as a real agent would; the server answers with a
/// plain refusal, so its message is the only sign of this case.
fn is_contended(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<Error>(),
        Some(Error::Server(body))
            if body.code == ErrorCode::Refused && body.message.starts_with("base kept moving")
    )
}

/// A pause that grows with the land's rounds and differs between agents,
/// so agents the server turned away together do not return together.
fn contention_backoff(agent: usize, round: u32) -> Duration {
    let mixed =
        (agent as u64 + 1).wrapping_mul(2_654_435_761) ^ u64::from(round).wrapping_mul(40_503);
    let spread = mixed % 250;
    Duration::from_millis(100 * u64::from(round.min(10)) + spread)
}

/// A workspace directory for try `tries` of change `n` that no earlier try
/// used. Each try in a process starts its search at its own number, so a
/// timed-out try still writing its directory never shares it with the next.
fn fresh_root(agent_dir: &std::path::Path, n: usize, tries: u32) -> PathBuf {
    let first = agent_dir.join(format!("{n:04}"));
    if tries == 0 && !first.exists() {
        return first;
    }
    (u64::from(tries).max(1)..)
        .map(|k| agent_dir.join(format!("{n:04}.{k}")))
        .find(|root| !root.exists())
        .expect("some directory name is free")
}

/// Errors a lost connection or a restarting daemon causes, as opposed to
/// a refusal that repeating would only repeat.
fn is_transient(err: &Error) -> bool {
    use std::io::ErrorKind;
    match err {
        Error::Disconnected | Error::NotRunning(_) => true,
        // The change was made but the outage cut its workspace short. It
        // is left open and empty; the next try makes another change.
        Error::WorkspaceNotCreated { source, .. } => is_transient(source),
        // A cache daemon that cannot reach the server, or a server on its
        // way down: both answer again once the outage ends (D8).
        Error::Server(body) => {
            matches!(body.code, ErrorCode::Unavailable | ErrorCode::ShuttingDown)
        }
        Error::Io(io) => matches!(
            io.kind(),
            ErrorKind::BrokenPipe
                | ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected
                | ErrorKind::UnexpectedEof
                | ErrorKind::TimedOut
        ),
        _ => false,
    }
}
