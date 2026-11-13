//! Typed calls for review threads (SPEC R7). Velocity's PR duty reviews
//! through these.

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{
    CommentRequest, Request, ResolveRequest, Response, ReviewThread, ThreadFilter,
};

impl Client {
    /// Approve exactly the head read by the reviewer.
    pub async fn review_approve(
        &self,
        request: rosen_protocol::ApproveRequest,
    ) -> Result<rosen_protocol::Approval, Error> {
        match self.request(Request::ReviewApprove(request)).await? {
            Response::Approval(approval) => Ok(approval),
            other => Err(unexpected(other)),
        }
    }

    /// Includes invalidated approvals so review history remains visible.
    pub async fn review_approvals(
        &self,
        change: &str,
    ) -> Result<Vec<rosen_protocol::Approval>, Error> {
        match self
            .request(Request::ReviewApprovals {
                change: change.into(),
            })
            .await?
        {
            Response::Approvals(approvals) => Ok(approvals),
            other => Err(unexpected(other)),
        }
    }

    /// Starts a thread on a line of the change's head, or replies in one.
    /// Answers with the whole thread, including the new comment.
    pub async fn review_comment(&self, request: CommentRequest) -> Result<ReviewThread, Error> {
        match self.request(Request::ReviewComment(request)).await? {
            Response::ReviewThread(thread) => Ok(thread),
            other => Err(unexpected(other)),
        }
    }

    /// Marks a thread resolved; resolving a resolved thread changes nothing.
    pub async fn review_resolve(&self, request: ResolveRequest) -> Result<ReviewThread, Error> {
        match self.request(Request::ReviewResolve(request)).await? {
            Response::ReviewThread(thread) => Ok(thread),
            other => Err(unexpected(other)),
        }
    }

    /// A change's threads with their comments, oldest first.
    pub async fn review_list(&self, filter: ThreadFilter) -> Result<Vec<ReviewThread>, Error> {
        match self.request(Request::ReviewList(filter)).await? {
            Response::ReviewThreads(threads) => Ok(threads),
            other => Err(unexpected(other)),
        }
    }
}
