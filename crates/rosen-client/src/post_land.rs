//! External post-land checker calls (SPEC R12).
use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{PostLandJob, PostLandWork, Request, Response};

impl Client {
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
