//! Typed call for reading the project configuration (SPEC "Project
//! configuration").

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_core::ObjectId;
use rosen_protocol::{ConfigQuery, ConfigRecord, Request, Response};

impl Client {
    /// The project configuration in force at `at`, or at the current base
    /// when `at` is `None`. Checkers read the check sets' commands here,
    /// and daemons the warm pool size.
    pub async fn project_config(&self, at: Option<ObjectId>) -> Result<ConfigRecord, Error> {
        match self
            .request(Request::ConfigShow(ConfigQuery { at }))
            .await?
        {
            Response::Config(record) => Ok(*record),
            other => Err(unexpected(other)),
        }
    }
}
