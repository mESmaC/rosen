//! One simulated agent: it creates changes with workspaces (some stacked),
//! edits them, claims paths, gets a receipt and an approval, and lands,
//! through `rosen-client` alone, as Velocity's agents will (§5, §9).

use crate::claims::HotClaims;
use crate::machine::Shared;
use crate::report::{ChangeOutcome, LandSample, StartSample};
use anyhow::{anyhow, bail, Result};
use rosen_client::protocol::{ApproveRequest, ClaimOutcome, ErrorCode, LandStatus};
use rosen_client::{AgentId, ChangeId, Error, ObjectId, Session, WorkspaceSpec};
use rosen_core::receipt::PRE_LAND_CHECK_SET;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Land requests one change may make before the agent gives up on it.
/// Each needs a fresh receipt because another change in its unit landed
/// first, so this bounds contention, not correctness.
const MAX_LAND_ROUNDS: u32 = 200;

/// Reconnect attempts after the daemon goes away, about a minute in all.
const MAX_RECONNECTS: u32 = 60;

pub struct Agent {
    pub index: usize,
    pub name: AgentId,
    pub session: Session,
    pub shared: Arc<Shared>,
}

/// A change this agent made and is carrying to the base.
struct Work {
    n: usize,
    change: ChangeId,
    root: PathBuf,
    stacked_on: Option<ChangeId>,
    expected: Vec<(String, String)>,
}

/// Retries a request across a lost connection, reconnecting between
/// tries. Only for requests that are safe to repeat: the server answers a
/// repeated land with `AlreadyLanded`, never a second land.
macro_rules! retrying {
    ($agent:expr, $what:expr, |$session:ident| $call:expr) => {{
        let mut tries = 0;
        loop {
            let $session = $agent.session.clone();
            match $call.await {
                Ok(value) => break Ok(value),
                Err(err) if is_transient(&err) && tries < MAX_RECONNECTS => {
                    tries += 1;
                    $agent.note(format!("{}: {err}; reconnecting", $what));
                    $agent.reconnect().await;
                }
                Err(err) => break Err(anyhow::Error::new(err).context($what)),
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
            self.run_batch(&batch).await;
        }
    }

    /// Makes one change per number, each stacked on the one before, then
    /// lands them bottom first (R8 step 3).
    async fn run_batch(&mut self, numbers: &[usize]) {
        let mut stack: Vec<Work> = Vec::new();
        for &n in numbers {
            let on = stack.last().map(|work| work.change);
            match self.start_change(n, on).await {
                Ok(work) => stack.push(work),
                Err(err) => {
                    self.note(format!("change {n} not started: {err:#}"));
                    break;
                }
            }
        }
        let mut blocked: Option<String> = None;
        for work in &stack {
            let result = match &blocked {
                Some(why) => Err(anyhow!("parent did not land: {why}")),
                None => self.land(work).await,
            };
            if let Err(err) = &result {
                blocked.get_or_insert_with(|| format!("{err:#}"));
            }
            self.finish(work, result).await;
        }
    }

    async fn start_change(&mut self, n: usize, on: Option<ChangeId>) -> Result<Work> {
        let shared = Arc::clone(&self.shared);
        let plan = &shared.plan;
        let root = shared
            .work_dir
            .join(plan.agent_name(self.index))
            .join(format!("{n:04}"));
        let mut spec = WorkspaceSpec::new(&root);
        if let Some(parent) = on {
            spec = spec.on(parent.to_string());
        }
        let started = Instant::now();
        let created = self.session.create_workspace(spec).await?;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let method = created.report.method.as_str().to_owned();
        self.shared
            .record(|report| report.workspace_starts.push(StartSample { ms, method }));
        let change = created.change.change.id;
        let expected = crate::edit::write_edits(plan, self.index, n, &root)?;
        self.claim(n, change).await?;
        let message = format!("{} change {n}", self.name);
        retrying!(self, "snapshot", |s| s
            .snapshot(&root, Some(message.clone())))?;
        Ok(Work {
            n,
            change,
            root,
            stacked_on: on,
            expected,
        })
    }

    /// Claims the change's own new file, shares its unit's slots file, and
    /// on some changes races the unit's other agents for the hot path.
    async fn claim(&mut self, n: usize, change: ChangeId) -> Result<()> {
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
        if plan.claims_hot(n) {
            let hot = crate::plan::Plan::hot_path(unit);
            let paths = vec![hot.clone()];
            match retrying!(self, "claim hot", |s| s.claim(&id, &paths))? {
                ClaimOutcome::Granted { .. } => self.hot_granted(change, &hot).await?,
                ClaimOutcome::Refused { .. } => {
                    self.shared.record(|report| report.hot_claims_refused += 1);
                }
            }
        }
        Ok(())
    }

    async fn hot_granted(&mut self, change: ChangeId, hot: &str) -> Result<()> {
        self.shared.record(|report| report.hot_claims_granted += 1);
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
            let merge = retrying!(self, "merge result", |s| s.client().merge_result(&id))?;
            if merge.tree.is_none() {
                bail!("merge conflict in {:?}", merge.conflicts);
            }
            if let Err(err) = self.receipt(&id, merge.head, merge.base).await {
                if !self.head_moved(&id, merge.head, &err).await {
                    return Err(err);
                }
                self.shared.record(|report| {
                    *report
                        .land_refusals
                        .entry("receipt_head_moved".into())
                        .or_default() += 1;
                });
                continue;
            }
            if approved != Some(merge.head) {
                self.approve(&id, merge.head).await?;
                approved = Some(merge.head);
            }
            let started = Instant::now();
            let answer = retrying!(self, "land", |s| s.client().land(&id))?;
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            match answer.outcome {
                LandStatus::Landed { commit, attempts } => {
                    let sample = LandSample {
                        change: work.change,
                        ms,
                        attempts,
                        rechecks: round,
                    };
                    self.shared.record(|report| report.lands.push(sample));
                    return Ok(commit);
                }
                LandStatus::AlreadyLanded { commit } => return Ok(commit),
                LandStatus::Refused {
                    reason, message, ..
                } => {
                    self.shared.record(|report| {
                        *report.land_refusals.entry(reason.clone()).or_default() += 1;
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

    /// The checker's part (D9): it would build and lint the merge result;
    /// the harness measures Rosen, not a build, so it receipts at once.
    async fn receipt(&mut self, id: &str, head: ObjectId, base: ObjectId) -> Result<()> {
        let checker = self.session.as_agent(self.shared.checker.clone());
        retrying!(self, "receipt", |_s| checker.submit_receipt(
            id,
            head,
            base,
            PRE_LAND_CHECK_SET
        ))?;
        Ok(())
    }

    /// Whether a refused receipt was refused only because the server
    /// rebased the change after its merge result was read (R8 step 4), so
    /// the agent should read the merge result again.
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
            stacked_on: work.stacked_on,
            expected: work.expected.clone(),
            workspace: work.root.clone(),
            landed: result.ok(),
            failure,
        };
        self.shared.record(|report| report.changes.push(outcome));
    }

    fn note(&self, message: String) {
        let line = format!("{}: {message}", self.name);
        self.shared.record(|report| report.errors.push(line));
    }

    fn violation(&self, message: String) {
        let line = format!("{}: {message}", self.name);
        self.shared
            .record(|report| report.claim_violations.push(line));
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
            .open()
            .await;
        if let Ok(session) = opened {
            self.session = session;
        }
    }
}

/// Errors a lost connection or a restarting daemon causes, as opposed to
/// a refusal that repeating would only repeat.
fn is_transient(err: &Error) -> bool {
    use std::io::ErrorKind;
    match err {
        Error::Disconnected | Error::NotRunning(_) => true,
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
        ),
        _ => false,
    }
}
