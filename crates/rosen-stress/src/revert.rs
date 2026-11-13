//! §5 "an injected failing commit is bisected and reverted, and its change
//! reopens with its work intact" (R12), checked against the server.

use crate::checker::CheckerReport;
use crate::plan::Plan;
use crate::report::ChangeOutcome;
use crate::verify::{is_server_revert, read_at, Failures};
use anyhow::{Context, Result};
use rosen_client::protocol::LogEntry;
use rosen_client::{Client, ObjectId};
use rosen_core::ChangeState;

pub struct RevertView<'a> {
    pub plan: &'a Plan,
    pub history: &'a [LogEntry],
    pub tip: ObjectId,
    pub culprit: Option<&'a ChangeOutcome>,
}

pub async fn check_revert(
    client: &Client,
    view: &RevertView<'_>,
    checker: Option<&CheckerReport>,
    failures: &mut Failures,
) -> Result<()> {
    let reverts: Vec<&LogEntry> = view
        .history
        .iter()
        .filter(|e| is_server_revert(e))
        .collect();
    if let Some(checker) = checker {
        check_checker(checker, view, failures);
    }
    let (Some(culprit), Some(path)) = (view.culprit, view.plan.failure_path()) else {
        if view.plan.culprit().is_some() {
            failures.push("the change carrying the injected failure never landed".into());
        }
        for revert in reverts {
            failures.push(format!(
                "the server reverted with {} though nothing failed",
                revert.commit
            ));
        }
        return Ok(());
    };
    let commit = culprit.landed.expect("the culprit landed");
    let expected = format!("Revert {commit}");
    let (right, wrong): (Vec<&LogEntry>, Vec<&LogEntry>) =
        reverts.iter().partition(|e| e.summary == expected);
    if right.len() != 1 {
        failures.push(format!(
            "the culprit {commit} was reverted {} times, not once",
            right.len()
        ));
    }
    for revert in wrong {
        failures.push(format!(
            "{} reverted something other than the culprit: {}",
            revert.commit, revert.summary
        ));
    }
    if read_at(client, &view.tip.to_string(), &path)
        .await?
        .is_some()
    {
        failures.push(format!(
            "the final base still holds the injected failure {path}"
        ));
    }
    check_reopened(client, culprit, failures).await
}

/// The culprit change is open again with its owner, and its head still
/// holds every line it wrote.
async fn check_reopened(
    client: &Client,
    culprit: &ChangeOutcome,
    failures: &mut Failures,
) -> Result<()> {
    let id = culprit.change.to_string();
    let record = client
        .change_show(&id)
        .await
        .context("reading the culprit change")?;
    if record.change.state != ChangeState::Open {
        failures.push(format!(
            "the culprit change {id} is {}, not reopened",
            record.change.state.as_str()
        ));
    }
    for (path, line) in &culprit.expected {
        let text = read_at(client, &id, path).await?;
        if !text.is_some_and(|text| text.lines().any(|l| l == line)) {
            failures.push(format!(
                "the reopened culprit {id} lost {line:?} from {path}"
            ));
        }
    }
    Ok(())
}

fn check_checker(checker: &CheckerReport, view: &RevertView<'_>, failures: &mut Failures) {
    if !checker.caught_up {
        failures.push("the post-land checker did not catch up with the base".into());
    }
    if checker.log.flaky > 0 {
        failures.push(format!("{} bisections stopped as flaky", checker.log.flaky));
    }
    let blamed: Vec<ObjectId> = checker.log.culprits.iter().map(|c| c.commit).collect();
    let culprit = view.culprit.and_then(|c| c.landed);
    if blamed.iter().any(|commit| Some(*commit) != culprit) {
        failures.push(format!(
            "bisection blamed {blamed:?}, not only the culprit {culprit:?}"
        ));
    }
}
