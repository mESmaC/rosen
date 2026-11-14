//! `rosen review comment|resolve|list` (SPEC R7, R10).

use crate::change_cmd::resolve_owner;
use crate::commands::ReviewCommand;
use crate::server::Succeeded;
use rosen_client::protocol::{
    Approval, ApproveRequest, CommentRequest, CommentTarget, ResolveRequest, ReviewThread,
    ThreadFilter,
};
use rosen_client::{Endpoint, Error};
use rosen_core::{ObjectId, ThreadState};
use serde_json::json;

use crate::output::REVIEW_LIST_SCHEMA as THREAD_LIST_SCHEMA;
use crate::output::REVIEW_THREAD_SCHEMA as THREAD_SCHEMA;

/// A review command with its arguments checked, ready to send.
enum ReviewCall {
    Comment(CommentRequest),
    Approve(ApproveRequest),
    Approvals(String),
    Resolve(ResolveRequest),
    List(ThreadFilter),
}

pub async fn run(
    json: bool,
    endpoint: &Endpoint,
    command: &ReviewCommand,
) -> Result<Succeeded, Error> {
    // Parse arguments before starting a daemon, so a typo costs nothing.
    let call = parse(command)?;
    let client = rosen_client::connect_or_start(endpoint).await?;
    match call {
        ReviewCall::Approve(req) => print_approval(json, &client.review_approve(req).await?),
        ReviewCall::Comment(req) => print_thread(json, &client.review_comment(req).await?),
        ReviewCall::Resolve(req) => print_thread(json, &client.review_resolve(req).await?),
        ReviewCall::Approvals(change) => {
            print_approvals(json, &client.review_approvals(&change).await?)
        }
        ReviewCall::List(filter) => print_list(json, &client.review_list(filter).await?),
    }
    Ok(true)
}

fn parse(command: &ReviewCommand) -> Result<ReviewCall, Error> {
    Ok(match command {
        ReviewCommand::Comment {
            change,
            path,
            line,
            message,
            thread,
            head,
            author,
        } => ReviewCall::Comment(CommentRequest {
            change: change.clone(),
            author: resolve_owner(author.as_deref())?,
            body: message.clone(),
            head: head.as_deref().map(parse_head).transpose()?,
            target: target(path.as_deref(), *line, *thread)?,
        }),
        ReviewCommand::Resolve { thread, by } => ReviewCall::Resolve(ResolveRequest {
            thread: *thread,
            by: resolve_owner(by.as_deref())?,
        }),
        ReviewCommand::List {
            change,
            approvals: true,
            ..
        } => ReviewCall::Approvals(change.clone()),
        ReviewCommand::List { change, state, .. } => ReviewCall::List(ThreadFilter {
            change: change.clone(),
            state: state.as_deref().map(parse_state).transpose()?,
        }),
        ReviewCommand::Approve {
            change,
            head,
            reviewer,
        } => ReviewCall::Approve(ApproveRequest {
            change: change.clone(),
            head: parse_head(head)?,
            reviewer: resolve_owner(reviewer.as_deref())?,
        }),
    })
}

/// Clap already requires either --thread or both --path and --line; this
/// turns that choice into the request's target.
fn target(
    path: Option<&str>,
    line: Option<u32>,
    thread: Option<u64>,
) -> Result<CommentTarget, Error> {
    match (thread, path, line) {
        (Some(thread), _, _) => Ok(CommentTarget::Thread { thread }),
        (None, Some(path), Some(line)) => Ok(CommentTarget::Line {
            path: path.to_owned(),
            line,
        }),
        _ => Err(Error::InvalidArgument(
            "pass --path and --line for a new thread, or --thread to reply".into(),
        )),
    }
}

fn parse_head(text: &str) -> Result<ObjectId, Error> {
    ObjectId::from_hex(text.trim()).map_err(|err| Error::InvalidArgument(err.to_string()))
}

fn parse_state(text: &str) -> Result<ThreadState, Error> {
    text.parse()
        .map_err(|err: rosen_core::Error| Error::InvalidArgument(err.to_string()))
}

fn print_thread(json: bool, thread: &ReviewThread) {
    if json {
        println!("{}", json!({ "schema": THREAD_SCHEMA, "thread": thread }));
    } else {
        print_thread_text(thread);
    }
}

fn print_list(json: bool, threads: &[ReviewThread]) {
    if json {
        println!(
            "{}",
            json!({ "schema": THREAD_LIST_SCHEMA, "threads": threads })
        );
        return;
    }
    if threads.is_empty() {
        println!("no review threads");
    }
    for thread in threads {
        print_thread_text(thread);
    }
}

fn print_thread_text(thread: &ReviewThread) {
    let resolved = match &thread.resolved_by {
        Some(by) => format!(" by {by}"),
        None => String::new(),
    };
    println!(
        "thread {} {}{resolved}: {}:{} at head {} of change {}",
        thread.id, thread.state, thread.path, thread.line, thread.head, thread.change
    );
    for comment in &thread.comments {
        // Mark replies written after the author moved the head, so a reader
        // knows the code may have changed since the thread started.
        let later = if comment.head == thread.head {
            String::new()
        } else {
            format!(" (at head {})", comment.head)
        };
        println!("  {}{later}: {}", comment.author, comment.body);
    }
}

fn print_approval(json: bool, approval: &Approval) {
    if json {
        println!(
            "{}",
            json!({"schema": crate::output::REVIEW_APPROVAL_SCHEMA, "approval": approval})
        );
    } else {
        println!(
            "approval {}: {} approved change {} at {}",
            approval.id,
            reviewer(approval),
            approval.change,
            approval.head
        );
    }
}

/// `<nick>@<machine>` when the approval's machine is known (R14).
fn reviewer(approval: &Approval) -> String {
    match &approval.machine {
        Some(machine) => format!("{}@{machine}", approval.reviewer),
        None => approval.reviewer.to_string(),
    }
}

fn print_approvals(json: bool, approvals: &[Approval]) {
    if json {
        println!(
            "{}",
            json!({"schema": crate::output::REVIEW_APPROVALS_SCHEMA, "approvals": approvals})
        );
    } else {
        for approval in approvals {
            let status = if approval.valid { "valid" } else { "void" };
            println!(
                "{}: {} approved {} at {} ({status})",
                approval.id,
                reviewer(approval),
                approval.change,
                approval.head
            );
        }
    }
}
