//! Typed call for `rosen resolve` (SPEC R5, R10).

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{ConflictsResolved, Request, ResolveConflicts, Response};

impl Client {
    /// Marks every open conflict in `paths` (repository-relative files) of
    /// `change` resolved, all or none. Fails, changing nothing, when a path
    /// holds no conflict stored on the change or the change is closed. The
    /// answer's `open_paths` lists the files still blocking a land.
    pub async fn resolve_conflicts(
        &self,
        change: &str,
        paths: Vec<String>,
    ) -> Result<ConflictsResolved, Error> {
        let request = Request::ConflictResolve(ResolveConflicts {
            change: change.to_owned(),
            paths,
        });
        match self.request(request).await? {
            Response::ConflictsResolved(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }
}
