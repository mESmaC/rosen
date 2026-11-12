//! `import` (SPEC R1): asks the server to load a git repository's history
//! and make one of its branches the base.

use std::path::Path;

use rosen_protocol::{ImportSummary, Request, Response};

use crate::{Client, Error};

impl Client {
    /// Imports `source`, a local repository path or a URL git can clone.
    /// `branch` defaults to the branch the source's HEAD names.
    pub async fn import(&self, source: &str, branch: Option<&str>) -> Result<ImportSummary, Error> {
        let request = Request::Import {
            source: absolute_source(source)?,
            branch: branch.map(str::to_owned),
        };
        match self.request(request).await? {
            Response::Imported(summary) => Ok(summary),
            other => Err(Error::Unexpected(format!("unexpected response {other:?}"))),
        }
    }
}

/// The server runs in its own directory, so a relative path must be made
/// absolute on the client, where it was typed. Anything that is not an
/// existing path is passed on unchanged as a URL.
fn absolute_source(source: &str) -> Result<String, Error> {
    let path = Path::new(source);
    if !path.exists() {
        return Ok(source.to_owned());
    }
    let absolute = std::path::absolute(path)?;
    Ok(absolute.to_string_lossy().into_owned())
}
