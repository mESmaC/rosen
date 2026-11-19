//! External post-land checker calls (SPEC R12).
use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{PostLandJob, PostLandStatus, PostLandWork, Request, Response};

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

    /// Submit only after all commands in the offered check set passed.
    /// Failures go to the bisection interface and never advance green.
    pub async fn post_land_passed(&self, job: PostLandJob) -> Result<PostLandWork, Error> {
        match self.request(Request::PostLandPassed(job)).await? {
            Response::PostLand(work) => Ok(work),
            other => Err(unexpected(other)),
        }
    }
}
