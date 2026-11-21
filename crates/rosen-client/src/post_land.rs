//! External post-land checker calls (SPEC R12).
use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{
    DeployRecord, PostLandJob, PostLandStatus, PostLandWork, Request, Response, SubmitDeployResult,
};

impl Client {
    /// Build/lint jobs after landing, independently of the test prefix.
    pub async fn post_land_build_next(&self) -> Result<PostLandWork, Error> {
        match self.request(Request::PostLandBuildNext).await? {
            Response::PostLand(work) => Ok(work),
            other => Err(unexpected(other)),
        }
    }

    /// Durable confirmed failures, including test failures that did not revert.
    pub async fn post_land_failures(
        &self,
        change: Option<String>,
    ) -> Result<Vec<rosen_protocol::PostLandFailure>, Error> {
        match self.request(Request::PostLandFailures { change }).await? {
            Response::PostLandFailures(failures) => Ok(failures),
            other => Err(unexpected(other)),
        }
    }

    /// Checkpoints, open bisections and pending inverses. Reads only: it
    /// never offers a job or moves a bisection.
    pub async fn post_land_status(&self) -> Result<PostLandStatus, Error> {
        match self.request(Request::PostLandStatus).await? {
            Response::PostLandStatus(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    pub async fn post_land_next(&self) -> Result<PostLandWork, Error> {
        match self.request(Request::PostLandNext).await? {
            Response::PostLand(work) => Ok(work),
            other => Err(unexpected(other)),
        }
    }

    /// Offers the newest untested landed commit matching `platform` (R12, #440).
    pub async fn post_land_next_for(&self, platform: &str) -> Result<PostLandWork, Error> {
        let request = Request::PostLandNextFor {
            platform: platform.to_owned(),
            kind: rosen_protocol::CheckKind::Test,
        };
        match self.request(request).await? {
            Response::PostLand(work) => Ok(work),
            other => Err(unexpected(other)),
        }
    }

    /// Build/lint jobs after landing matching `platform` (R12, #440).
    pub async fn post_land_build_next_for(&self, platform: &str) -> Result<PostLandWork, Error> {
        let request = Request::PostLandNextFor {
            platform: platform.to_owned(),
            kind: rosen_protocol::CheckKind::Build,
        };
        match self.request(request).await? {
            Response::PostLand(work) => Ok(work),
            other => Err(unexpected(other)),
        }
    }

    /// Submit only after all commands in the offered check set passed.
    /// Failures go to the bisection interface and never advance green.
    pub async fn post_land_passed(&self, job: PostLandJob) -> Result<PostLandWork, Error> {
        match self.request(Request::PostLandPassed(job)).await? {
            Response::PostLand(work) => Ok(work),
            other => Err(unexpected(other)),
        }
    }

    /// Records the outcome of a `deploy_due` step run by an external checker (R12).
    pub async fn deploy_result(&self, result: SubmitDeployResult) -> Result<DeployRecord, Error> {
        match self.request(Request::DeployResult(result)).await? {
            Response::DeployRecorded(record) => Ok(*record),
            other => Err(unexpected(other)),
        }
    }

    /// Materialises landed `commit`'s tree into `dest` for a checker to run in
    /// (R12, #440). Fetches missing objects into the local store first.
    pub async fn materialise_commit(
        &self,
        commit: rosen_core::ObjectId,
        dest: &std::path::Path,
    ) -> Result<crate::workspace::Report, Error> {
        self.ensure_local(commit).await?;
        let store = self.local_store().await?;
        let tree = rosen_store::ObjectStore::read_commit(&store, &commit)
            .map_err(|err| Error::Workspace(err.into()))?
            .tree;
        self.materialise_build_tree(tree, dest).await
    }
}
