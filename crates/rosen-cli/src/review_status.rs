//! `rosen review status`: R12 progress for people, or `rosen.post_land_status/v1`.
use rosen_client::protocol::{
    BisectSession, BisectStep, Checkpoint, PendingInverse, PostLandStatus, StepState,
};
use rosen_core::ObjectId;
use serde_json::json;

pub fn print(json_output: bool, status: &PostLandStatus) {
    if json_output {
        let schema = crate::output::POST_LAND_STATUS_SCHEMA;
        println!("{}", json!({ "schema": schema, "status": status }));
        return;
    }
    match &status.base {
        Some(base) => println!("base         {}", short(base)),
        None => println!("base         none: nothing imported yet"),
    }
    println!("last built   {}", checkpoint(status.last_built.as_ref()));
    println!("last green   {}", checkpoint(status.last_green.as_ref()));
    if !status.platforms.is_empty() {
        println!("platforms:");
        for platform in &status.platforms {
            println!("  {}", crate::server::platform_status_line(platform));
        }
    }
    if status.sessions.is_empty() {
        println!("no bisection in progress");
    }
    for session in &status.sessions {
        print_session(session);
    }
    for inverse in &status.pending_inverses {
        print_inverse(inverse);
    }
}

fn checkpoint(point: Option<&Checkpoint>) -> String {
    match point {
        Some(point) => format!("{}{}", short(&point.commit), by(&point.submitter, point.at)),
        None => "none yet".into(),
    }
}

/// ` by nick@machine at time`, or nothing for results from before R14 attribution.
fn by(submitter: &Option<String>, at: Option<i64>) -> String {
    let mut text = String::new();
    if let Some(submitter) = submitter {
        text.push_str(&format!(" by {submitter}"));
    }
    if let Some(at) = at {
        let seconds = u64::try_from(at / 1000).unwrap_or(0);
        text.push_str(&format!(" at {}", crate::machine_list::utc_minute(seconds)));
    }
    text
}

fn print_session(session: &BisectSession) {
    let progress = if session.flaky {
        "stopped as flaky: contradictory confirmation, investigate the batch".to_owned()
    } else {
        let current = session
            .steps
            .iter()
            .find(|step| step.state == StepState::Offered)
            .map_or(0, |step| step.step + 1);
        format!("step {current} of {}", session.planned_steps)
    };
    let platform = session
        .platform
        .as_deref()
        .map(|p| format!(" ({})", rosen_core::Platform::display_label(p)))
        .unwrap_or_default();
    println!(
        "bisecting {} failure{platform} {} (session {}): {} commit(s) after {} up to {}, {progress}",
        session.kind.as_str(),
        short(&session.failing),
        session.id,
        session.interval,
        short(&session.lower),
        short(&session.failing),
    );
    println!(
        "  opened{}",
        by(&session.opened_by, session.opened_at).trim_end()
    );
    if !session.failing_tests.is_empty() {
        println!("  failing tests: {}", session.failing_tests.join(", "));
    }
    for step in &session.steps {
        print_step(step);
    }
}

fn print_step(step: &BisectStep) {
    let commit = step.commit.as_ref().map_or("-".to_owned(), short);
    let state = match step.state {
        StepState::Waiting => "waiting",
        StepState::Offered => "offered",
        StepState::Passed => "passed",
        StepState::Failed => "failed",
    };
    println!(
        "  step {:<3} {commit:<12} {state:<8}{}",
        step.step + 1,
        by(&step.submitter, step.at)
    );
}

fn print_inverse(inverse: &PendingInverse) {
    println!(
        "inverse of {} ({} failure, change {}) waits to land",
        short(&inverse.culprit),
        inverse.kind.as_str(),
        inverse.change
    );
    let Some(base) = &inverse.attempted_base else {
        return;
    };
    let first = by(&None, inverse.first_refused_at);
    println!("  refused onto {}{first}", short(base));
    if let Some(reason) = &inverse.reason {
        println!("  reason: {reason}");
    }
    if !inverse.paths.is_empty() {
        println!("  conflicting paths: {}", inverse.paths.join(", "));
    }
    if !inverse.overlapping_commits.is_empty() {
        let commits: Vec<String> = inverse.overlapping_commits.iter().map(short).collect();
        println!("  overlapping later commits: {}", commits.join(", "));
    }
}

fn short(commit: &ObjectId) -> String {
    commit.to_hex()[..12].to_owned()
}
