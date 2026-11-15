//! The operator's mirror repair command (SPEC R1).
use rosen_client::{Client, Endpoint, Error};
use serde_json::json;

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
