//! Review, checks, landing and rebases through a session (SPEC.md R2, R6,
//! R7, R8). These are the steps Velocity ran through GitHub PRs and its own
//! merge flow: PR duty reviews and approves, the build queue checks a merge
//! result and submits a receipt, and the author lands.

use super::Session;
use crate::workspace::{Options, Report};
use crate::Error;
use rosen_core::{ObjectId, ThreadState};
use rosen_protocol::{
    Approval, ApproveRequest, ChangeRecord, LandAnswer, MergeResultAnswer, OverlapAnswer,
    OverlapQuery, ReviewThread, ThreadFilter,
};
use std::path::{Path, PathBuf};

/// A change's merge result, written out for a checker to build (D9).
#[derive(Debug, Clone)]
pub struct MergeCheckout {
    /// The head, base and tree a receipt for this build is bound to.
    pub merge: MergeResultAnswer,
    /// Where the merged files are, or `None` when the merge conflicts and
    /// nothing was written: there is nothing to check until it is resolved.
    pub dir: Option<PathBuf>,
    pub report: Option<Report>,
}

/// Review (R7), recorded under this agent as reviewer.
impl Session {
    /// Approves exactly `head`, the head this agent reviewed. The server
    /// refuses a stale head, so an approval never covers unread work.
    pub async fn approve(&self, change: &str, head: ObjectId) -> Result<Approval, Error> {
        let request = ApproveRequest {
            change: change.to_owned(),
            reviewer: self.agent.clone(),
            head,
        };
        self.client.review_approve(request).await
    }

    /// Every approval the change has had, void ones included.
    pub async fn approvals(&self, change: &str) -> Result<Vec<Approval>, Error> {
        self.client.review_approvals(change).await
    }

    /// The change's review threads, only those in `state` when given.
    pub async fn threads(
        &self,
        change: &str,
        state: Option<ThreadState>,
    ) -> Result<Vec<ReviewThread>, Error> {
        let filter = ThreadFilter {
            change: change.to_owned(),
            state,
        };
        self.client.review_list(filter).await
    }
}

/// Checks (R8 step 1) and landing (R8 step 2).
impl Session {
    /// Merges the change onto the current base on the server. The answer's
    /// head and base are what a receipt for it must name.
    pub async fn merge_result(&self, change: &str) -> Result<MergeResultAnswer, Error> {
        self.client.merge_result(change).await
    }

    /// Merges the change onto the current base and writes the merged tree
    /// into the new directory `dir`, for a checker to build in. The
    /// directory is a plain build tree, not a workspace: nothing in it is
    /// snapshotted, and the checker removes it when done.
    pub async fn check_out_merge_result(
        &self,
        change: &str,
        dir: &Path,
    ) -> Result<MergeCheckout, Error> {
        let merge = self.client.merge_result(change).await?;
        let Some(tree) = merge.tree else {
            return Ok(MergeCheckout {
                merge,
                dir: None,
                report: None,
            });
        };
        let report = self
            .client
            .materialise_tree(tree, dir, Options::default())
            .await?;
        Ok(MergeCheckout {
            merge,
            dir: Some(dir.to_path_buf()),
            report: Some(report),
        })
    }

    /// Asks the server to land the change. A refusal is an answer naming
    /// its reason, not an error.
    ///
    /// When the connection drops mid-land the caller cannot know whether
    /// the land committed, so this asks again on fresh connections, starting
    /// the daemon if it went down. The server answers a repeat with
    /// `already_landed`, so the land is never applied twice (R8).
    pub async fn land(&self, change: &str) -> Result<LandAnswer, Error> {
        match self.client.land(change).await {
            Err(Error::Disconnected | Error::Io(_)) => {
                crate::land_reconnecting(&self.endpoint, change).await
            }
            settled => settled,
        }
    }
}

/// Rebases (R2), overlaps (R6) and the warm pool (R3).
impl Session {
    /// Asks the server to rebase the change onto the current base now,
    /// rather than at its landing time. Its local workspaces change only when
    /// their owner calls [`Session::apply_rebase`].
    pub async fn rebase(&self, change: &str) -> Result<ChangeRecord, Error> {
        self.client.rebase_change(change).await
    }

    /// Writes the server's rebased tree into this agent's workspace at
    /// `root`, in place of `airc_rebase`. Call it at a step boundary or
    /// while the agent is parked, with its tools and the workspace's watcher
    /// stopped. It refuses while any edit is not in a stored snapshot.
    pub async fn apply_rebase(&self, root: &Path) -> Result<ObjectId, Error> {
        self.client.apply_rebase(root, &self.agent).await
    }

    /// Every live change that overlaps `change`'s claims, shares and edits,
    /// or `paths` queried as a claim. With both, only `change`'s overlaps on
    /// those paths.
    pub async fn overlaps(
        &self,
        change: Option<&str>,
        paths: &[String],
    ) -> Result<OverlapAnswer, Error> {
        let query = OverlapQuery {
            change: change.map(str::to_owned),
            paths: paths.to_vec(),
        };
        self.client.overlaps(query).await
    }

    /// Sets this machine's warm-pool target, as a swarm launch does before
    /// taking many workspaces at once. Zero turns the pool off.
    pub async fn set_pool_size(&self, size: usize) -> Result<(), Error> {
        self.client.set_pool_size(size).await
    }
}
