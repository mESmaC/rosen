//! Machine and join-key administration (SPEC R14).
use crate::{Client, Error};
use rosen_protocol::{JoinKeyRecord, MachineRecord, Request, Response};

impl Client {
    pub async fn machine_list(&self) -> Result<Vec<MachineRecord>, Error> {
        match self.request(Request::MachineList).await? {
            Response::Machines(value) => Ok(value),
            other => Err(crate::connection::unexpected(other)),
        }
    }
    pub async fn machine_approve(&self, machine: &str) -> Result<MachineRecord, Error> {
        match self
            .request(Request::MachineApprove {
                machine: machine.into(),
            })
            .await?
        {
            Response::Machine(value) => Ok(value),
            other => Err(crate::connection::unexpected(other)),
        }
    }
    pub async fn machine_revoke(&self, machine: &str) -> Result<MachineRecord, Error> {
        match self
            .request(Request::MachineRevoke {
                machine: machine.into(),
            })
            .await?
        {
            Response::Machine(value) => Ok(value),
            other => Err(crate::connection::unexpected(other)),
        }
    }
    pub async fn join_key_list(&self) -> Result<Vec<JoinKeyRecord>, Error> {
        match self.request(Request::JoinKeyList).await? {
            Response::JoinKeys(value) => Ok(value),
            other => Err(crate::connection::unexpected(other)),
        }
    }
    pub async fn join_key_revoke(&self, key: &str) -> Result<JoinKeyRecord, Error> {
        match self
            .request(Request::JoinKeyRevoke { key: key.into() })
            .await?
        {
            Response::JoinKey(value) => Ok(value),
            other => Err(crate::connection::unexpected(other)),
        }
    }
}
