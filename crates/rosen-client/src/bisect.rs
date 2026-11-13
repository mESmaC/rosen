//! Bisection checkers fetch commits through the history API, without a checkout.
use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{BisectResult, BisectState, Request, Response};

impl Client {
    pub async fn bisect_next(&self, session: i64) -> Result<BisectState, Error> {
        match self.request(Request::BisectNext { session }).await? {
            Response::Bisection(state) => Ok(state),
            other => Err(unexpected(other)),
        }
    }

    pub async fn bisect_submit(&self, result: BisectResult) -> Result<BisectState, Error> {
        match self.request(Request::BisectSubmit(result)).await? {
            Response::Bisection(state) => Ok(state),
            other => Err(unexpected(other)),
        }
    }
}
