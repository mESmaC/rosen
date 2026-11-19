//! The operator's mirror repair commands (SPEC R1).
use rosen_client::protocol::{CatchUpStatus, MirrorCatchUp};
use rosen_client::{Client, Endpoint, Error};
use rosen_core::ObjectId;
use serde_json::json;

pub async fn adopt(json: bool, endpoint: &Endpoint, commit: ObjectId) -> Result<bool, Error> {
    let client = Client::connect(endpoint).await?;
    let answer = client.mirror_adopt(commit).await?;
    if json {
        println!(
            "{}",
            json!({ "schema": crate::output::MIRROR_ADOPT_SCHEMA, "adoption": answer })
        );
    } else if answer.old_base == answer.new_base {
        println!(
            "{} already contains mirror commit {}",
            answer.base_ref, answer.remote_tip
        );
    } else {
        println!(
            "adopted mirror commit {} on {} at {}; a fast-forward push is queued",
            answer.remote_tip, answer.base_ref, answer.new_base
        );
    }
    Ok(true)
}

pub async fn catch_up(json: bool, endpoint: &Endpoint) -> Result<bool, Error> {
    // Never start a daemon implicitly: catching up an empty new server
    // would hide that the operator picked the wrong endpoint.
    let client = Client::connect(endpoint).await?;
    let answer = client.mirror_catch_up().await?;
    if json {
        println!(
            "{}",
            json!({ "schema": crate::output::MIRROR_CATCH_UP_SCHEMA, "catch_up": answer })
        );
        return Ok(answer.rebase_errors.is_empty());
    }
    match answer.status {
        CatchUpStatus::UpToDate => println!(
            "{} is already at mirror commit {}",
            answer.base_ref, answer.remote_tip
        ),
        CatchUpStatus::Ahead => println!(
            "{} at {} already contains mirror commit {}; the next push sends the rest",
            answer.base_ref, answer.old_base, answer.remote_tip
        ),
        CatchUpStatus::FastForwarded => print_fast_forward(&answer),
    }
    Ok(answer.rebase_errors.is_empty())
}

fn print_fast_forward(answer: &MirrorCatchUp) {
    println!(
        "fast-forwarded {} from {} to {} ({} imported commits)",
        answer.base_ref,
        answer.old_base,
        answer.new_base,
        answer.commits.len()
    );
    println!(
        "rebased {} open changes, {} with conflicts",
        answer.rebased.len(),
        answer.conflicted.len()
    );
    for change in &answer.conflicted {
        println!("  conflict: {change}");
    }
    for failed in &answer.rebase_errors {
        eprintln!("  not rebased {}: {}", failed.change, failed.error);
    }
}

pub async fn drop_paths(json: bool, endpoint: &Endpoint, paths: &[String]) -> Result<bool, Error> {
    // Never start a daemon implicitly: an operator must select the server
    // whose history is being repaired, rather than create an empty one.
    let client = Client::connect(endpoint).await?;
    let answer = client.mirror_drop_paths(paths.to_vec()).await?;
    if json {
        println!(
            "{}",
            json!({ "schema": crate::output::MIRROR_REWRITE_SCHEMA, "rewrite": answer })
        );
    } else {
        println!(
            "rewrote {} unmirrored commits on {}: {} -> {}; updated {} open changes",
            answer.rewritten_commits,
            answer.base_ref,
            answer.old_base,
            answer.new_base,
            answer.updated_changes
        );
        println!(
            "apply server rebases in open workspaces; the mirror resumes with a fast-forward push"
        );
    }
    Ok(true)
}
