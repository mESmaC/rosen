//! Typed calls for per-agent changes (SPEC R2): create, list, show, abandon.

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::{ChangeFilter, ChangeRecord, NewChange, Request, Response};

impl Client {
    /// Creates a change owned by `request.owner`, on the base or stacked on
    /// `request.on`. The returned ID stays the change's ID for its whole life.
    pub async fn change_new(&self, request: NewChange) -> Result<ChangeRecord, Error> {
        expect_change(self.request(Request::ChangeNew(request)).await?)
    }

    pub async fn change_list(&self, filter: ChangeFilter) -> Result<Vec<ChangeRecord>, Error> {
        match self.request(Request::ChangeList(filter)).await? {
            Response::Changes(changes) => Ok(changes),
            other => Err(unexpected(other)),
        }
    }

    /// `change` is a full change ID or a unique prefix of one.
    pub async fn change_show(&self, change: &str) -> Result<ChangeRecord, Error> {
        let change = change.to_owned();
        expect_change(self.request(Request::ChangeShow { change }).await?)
    }

    pub async fn change_abandon(&self, change: &str) -> Result<ChangeRecord, Error> {
        let change = change.to_owned();
        expect_change(self.request(Request::ChangeAbandon { change }).await?)
    }
}

fn expect_change(response: Response) -> Result<ChangeRecord, Error> {
    match response {
        Response::Change(record) => Ok(record),
        other => Err(unexpected(other)),
    }
}
