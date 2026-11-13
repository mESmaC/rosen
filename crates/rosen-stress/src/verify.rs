//! `rosen-stress verify`: merges every machine's report and checks §5's
//! guarantees against the server's base, then prints the measurements.

use crate::checker::CheckerReport;
use crate::machine::base_tip;
use crate::plan::Plan;
use crate::report::{percentile, ChangeOutcome, Report};
use anyhow::{bail, Context, Result};
use rosen_client::protocol::{LogEntry, LogRequest, ReadAnswer, ReadRequest};
use rosen_client::{ChangeId, Client, ObjectId};
use std::collections::{BTreeMap, HashMap, HashSet};

/// §5's latency targets.
const START_P95_MS: f64 = 2000.0;
const LAND_P95_MS: f64 = 200.0;

/// Log entries asked for per page while walking the base.
const LOG_PAGE: u32 = 500;

/// The author of the server's own reverts (R12).
const SERVER_AUTHOR: &str = "rosen-server <server@rosen>";

/// A §5 guarantee or target the run missed.
pub type Failures = Vec<String>;

/// Which of §5's targets a run is held to.
pub struct Targets<'a> {
    /// The p95 latency targets. A faulted run measures latency through
    /// restarts and outages, so it is held to the guarantees only.
    pub latency: bool,
    /// The run's post-land checker, when this process ran it.
    pub checker: Option<&'a CheckerReport>,
}

/// Everything the checks need, read once.
struct Run<'a> {
    plan: Plan,
    reports: &'a [Report],
    history: Vec<LogEntry>,
    tip: ObjectId,
    /// The culprit change's outcome, when the plan injected a failure.
    culprit: Option<&'a ChangeOutcome>,
}

pub async fn verify(
    client: &Client,
    reports: &[Report],
    targets: &Targets<'_>,
) -> Result<Failures> {
    let plan = common_plan(reports)?;
    let mut failures = Failures::new();
    let tip = base_tip(client).await.context("reading the base tip")?;
    let starts: HashSet<ObjectId> = reports.iter().filter_map(|r| r.base_at_start).collect();
    let history = walk_base(client, tip, &starts)
        .await
        .context("walking the base")?;
    let culprit = plan.culprit().and_then(|(agent, n)| {
        landed(reports).find(|change| change.index == agent && change.n == n)
    });
    let run = Run {
        plan,
        reports,
        history,
        tip,
        culprit,
    };
    check_history(&run, &mut failures);
    check_lost_work(client, &run, &mut failures).await?;
    check_gates(client, &run, &mut failures).await?;
    crate::revert::check_revert(client, &run.revert_view(), targets.checker, &mut failures).await?;
    check_measurements(&run, targets, &mut failures);
    Ok(failures)
}

impl Run<'_> {
    fn revert_view(&self) -> crate::revert::RevertView<'_> {
        crate::revert::RevertView {
            plan: &self.plan,
            history: &self.history,
            tip: self.tip,
            culprit: self.culprit,
        }
    }
}

fn landed(reports: &[Report]) -> impl Iterator<Item = &ChangeOutcome> {
    reports
        .iter()
        .flat_map(|report| &report.changes)
        .filter(|change| change.landed.is_some())
}

fn common_plan(reports: &[Report]) -> Result<Plan> {
    let Some(plan) = reports.first().and_then(|report| report.plan.clone()) else {
        bail!("no reports to verify");
    };
    if reports
        .iter()
        .any(|report| report.plan.as_ref() != Some(&plan))
    {
        bail!("the reports come from runs with different plans");
    }
    Ok(plan)
}

/// The base's commits newer than every machine's starting tip, newest
/// first. Landed history is linear (R8), so first parents are the history.
async fn walk_base(
    client: &Client,
    tip: ObjectId,
    starts: &HashSet<ObjectId>,
) -> Result<Vec<LogEntry>> {
    let mut seen_starts = HashSet::new();
    let mut history = Vec::new();
    let mut at = Some(tip);
    while let Some(commit) = at.take() {
        let request = LogRequest {
            at: Some(commit.to_string()),
            paths: Vec::new(),
            limit: Some(LOG_PAGE),
        };
        let page = client.log(request).await.context("reading the base log")?;
        for entry in page.entries {
            if starts.contains(&entry.commit) {
                seen_starts.insert(entry.commit);
            }
            if seen_starts.len() == starts.len() {
                return Ok(history);
            }
            at = entry.parents.first().copied();
            history.push(entry);
        }
        if !page.truncated {
            break;
        }
    }
    bail!("walked the whole base without reaching every machine's starting tip")
}

/// A commit the server landed itself to revert a culprit (R12): it has no
/// receipt or approval and the server as its author.
pub fn is_server_revert(entry: &LogEntry) -> bool {
    entry.receipt.is_none()
        && entry.approvers.is_empty()
        && entry.author == SERVER_AUTHOR
        && entry.summary.starts_with("Revert ")
}

/// No land applied twice; the base moved only by the run's own lands and
/// the server's reverts; and every land an agent was told about is on the
/// base, as that commit.
fn check_history(run: &Run, failures: &mut Failures) {
    let landed: HashMap<ChangeId, ObjectId> = landed(run.reports)
        .filter_map(|change| change.landed.map(|commit| (change.change, commit)))
        .collect();
    let mut on_base: HashMap<ChangeId, ObjectId> = HashMap::new();
    for entry in run.history.iter().filter(|entry| !is_server_revert(entry)) {
        let Some(change) = entry.change else {
            failures.push(format!("base commit {} carries no change", entry.commit));
            continue;
        };
        if on_base.insert(change, entry.commit).is_some() {
            failures.push(format!("change {change} was applied to the base twice"));
        }
        if !landed.contains_key(&change) {
            failures.push(format!(
                "base commit {} lands {change}, which no agent landed",
                entry.commit
            ));
        }
    }
    for (change, commit) in &landed {
        match on_base.get(change) {
            Some(found) if found == commit => {}
            Some(found) => failures.push(format!(
                "{change} landed as {commit} but the base has {found}"
            )),
            None => failures.push(format!(
                "{change} reported landed as {commit} but is not on the base"
            )),
        }
    }
}

/// Every line a landed change wrote is in the final base (§5 "no edit is
/// lost"). Later changes only ever rewrite other slots, so nothing a landed
/// change wrote should be gone, except the reverted culprit's.
async fn check_lost_work(client: &Client, run: &Run<'_>, failures: &mut Failures) -> Result<()> {
    let mut files: HashMap<String, Option<String>> = HashMap::new();
    let culprit = run.culprit.map(|change| change.change);
    for change in landed(run.reports).filter(|change| Some(change.change) != culprit) {
        for (path, line) in &change.expected {
            if !files.contains_key(path) {
                let text = read_at(client, &run.tip.to_string(), path).await?;
                files.insert(path.clone(), text);
            }
            let present = files[path]
                .as_deref()
                .is_some_and(|text| text.lines().any(|l| l == line));
            if !present {
                failures.push(format!(
                    "lost edit of {}: {path} lacks {line:?}",
                    change.change
                ));
            }
        }
    }
    Ok(())
}

/// The file at `at`, a commit or a change; `None` when it is not there.
pub async fn read_at(client: &Client, at: &str, path: &str) -> Result<Option<String>> {
    let request = ReadRequest {
        at: Some(at.to_owned()),
        path: path.to_owned(),
    };
    match client.read_file(request).await {
        Ok(ReadAnswer::File { data, .. }) => Ok(Some(String::from_utf8_lossy(&data).into_owned())),
        Ok(ReadAnswer::Directory { .. }) => Ok(None),
        Err(rosen_client::Error::Server(_)) => Ok(None),
        Err(err) => Err(err).context(format!("reading {path}")),
    }
}

/// The base moved only with a matching receipt and approval (§5): each
/// agent land's receipt trailer is one the run's checker gave for that
/// change's head and base, and a run reviewer approved that same head.
async fn check_gates(client: &Client, run: &Run<'_>, failures: &mut Failures) -> Result<()> {
    let reviewers: HashSet<String> = run
        .reports
        .iter()
        .map(|r| run.plan.reviewer_name(&r.machine))
        .collect();
    let receipts: HashSet<(ChangeId, ObjectId, ObjectId)> = run
        .reports
        .iter()
        .flat_map(|r| &r.receipts)
        .map(|r| (r.change, r.head, r.base))
        .collect();
    for entry in run.history.iter().filter(|entry| !is_server_revert(entry)) {
        let (Some(change), Some(receipt)) = (entry.change, &entry.receipt) else {
            failures.push(format!(
                "base commit {} landed with no receipt",
                entry.commit
            ));
            continue;
        };
        if !receipts.contains(&(change, receipt.head, receipt.base)) {
            failures.push(format!(
                "{change} landed as {} on a receipt the run never gave",
                entry.commit
            ));
        }
        let approvals = client
            .review_approvals(&change.to_string())
            .await
            .context("reading approvals")?;
        let approved = approvals
            .iter()
            .any(|a| a.head == receipt.head && reviewers.contains(a.reviewer.as_str()));
        if !approved {
            failures.push(format!(
                "{change} landed as {} with no run approval of its receipted head",
                entry.commit
            ));
        }
    }
    Ok(())
}

fn check_measurements(run: &Run, targets: &Targets, failures: &mut Failures) {
    let reports = run.reports;
    let planned = run.plan.agents * run.plan.lands_per_agent;
    let landed = landed(reports).count();
    if landed < planned {
        failures.push(format!("{landed} of {planned} planned changes landed"));
    }
    let ran: usize = reports.iter().map(|r| r.agent_count).sum();
    if ran != run.plan.agents {
        failures.push(format!(
            "reports cover {ran} agents of the plan's {}",
            run.plan.agents
        ));
    }
    for report in reports {
        failures.extend(
            report
                .claim_violations
                .iter()
                .map(|v| format!("claim: {v}")),
        );
    }
    if !targets.latency {
        return;
    }
    let starts: Vec<f64> = reports
        .iter()
        .flat_map(|r| &r.workspace_starts)
        .map(|s| s.ms)
        .collect();
    if let Some(p95) = percentile(&starts, 0.95).filter(|p95| *p95 >= START_P95_MS) {
        failures.push(format!(
            "workspace start p95 is {p95:.0} ms, over {START_P95_MS} ms"
        ));
    }
    let lands: Vec<f64> = reports
        .iter()
        .flat_map(|r| &r.lands)
        .map(|l| l.ms)
        .collect();
    if let Some(p95) = percentile(&lands, 0.95).filter(|p95| *p95 >= LAND_P95_MS) {
        failures.push(format!("land p95 is {p95:.1} ms, over {LAND_P95_MS} ms"));
    }
}

/// The measurements, for a person reading the run's output.
pub fn summary(reports: &[Report]) -> String {
    let mut out = String::new();
    let starts: Vec<f64> = reports
        .iter()
        .flat_map(|r| &r.workspace_starts)
        .map(|s| s.ms)
        .collect();
    out.push_str(&line("workspace start (ms)", &starts));
    let mut by_method: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for sample in reports.iter().flat_map(|r| &r.workspace_starts) {
        by_method.entry(&sample.method).or_default().push(sample.ms);
    }
    for (method, samples) in &by_method {
        out.push_str(&line(&format!("  via {method}"), samples));
    }
    let lands: Vec<f64> = reports
        .iter()
        .flat_map(|r| &r.lands)
        .map(|l| l.ms)
        .collect();
    out.push_str(&line("land (ms)", &lands));
    out.push_str(&counters(reports));
    out
}

/// Contention and failure counts: how hard the run pushed, and what it hit.
fn counters(reports: &[Report]) -> String {
    let mut out = String::new();
    let rechecks: u32 = reports
        .iter()
        .flat_map(|r| &r.lands)
        .map(|l| l.rechecks)
        .sum();
    let attempts: u32 = reports
        .iter()
        .flat_map(|r| &r.lands)
        .map(|l| l.attempts)
        .sum();
    let landed = landed(reports).count();
    out.push_str(&format!(
        "changes landed {landed}, re-receipts {rechecks}, server CAS attempts {attempts}\n"
    ));
    let mut refusals: BTreeMap<&str, u64> = BTreeMap::new();
    for (reason, count) in reports.iter().flat_map(|r| &r.land_refusals) {
        *refusals.entry(reason).or_default() += count;
    }
    out.push_str(&format!("land refusals {refusals:?}\n"));
    let granted: u64 = reports.iter().map(|r| r.hot_claims_granted).sum();
    let refused: u64 = reports.iter().map(|r| r.hot_claims_refused).sum();
    out.push_str(&format!(
        "hot claims granted {granted}, refused {refused}\n"
    ));
    let elapsed = reports.iter().map(|r| r.elapsed_ms).max().unwrap_or(0);
    let restarts: u32 = reports.iter().map(|r| r.restarts).sum();
    out.push_str(&format!(
        "slowest machine {:.1} s, agent process restarts {restarts}\n",
        elapsed as f64 / 1000.0
    ));
    let kept: Vec<&String> = reports.iter().flat_map(|r| &r.workspaces_kept).collect();
    out.push_str(&format!(
        "closed changes' workspaces not deleted {}\n",
        kept.len()
    ));
    for workspace in kept.iter().take(5) {
        out.push_str(&format!("  {workspace}\n"));
    }
    let errors: Vec<&String> = reports.iter().flat_map(|r| &r.errors).collect();
    out.push_str(&format!("errors {}\n", errors.len()));
    for error in errors.iter().take(10) {
        out.push_str(&format!("  {error}\n"));
    }
    out
}

fn line(label: &str, samples: &[f64]) -> String {
    let p = |fraction| percentile(samples, fraction).unwrap_or(f64::NAN);
    format!(
        "{label}: n={} p50={:.1} p95={:.1} max={:.1}\n",
        samples.len(),
        p(0.5),
        p(0.95),
        p(1.0)
    )
}
