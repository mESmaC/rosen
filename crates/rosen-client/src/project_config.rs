//! Typed call for reading the project configuration (SPEC "Project
//! configuration").

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_core::{ObjectId, ProjectConfig};
use rosen_protocol::{ConfigQuery, ConfigRecord, ErrorCode, Request, Response};

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

    /// The configuration in force at the current base, or the defaults
    /// before the project has a base: nothing configured can apply yet.
    pub async fn base_config(&self) -> Result<ProjectConfig, Error> {
        match self.project_config(None).await {
            Ok(record) => Ok(record.config),
            Err(Error::Server(body)) if body.code == ErrorCode::Refused => {
                Ok(ProjectConfig::default())
            }
            Err(error) => Err(error),
        }
    }
}
