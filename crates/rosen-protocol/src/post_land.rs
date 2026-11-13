//! External full-test checker interface (SPEC R12).
use rosen_core::{project_config::CheckSet, ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandJob {
    pub commit: ObjectId,
    /// Configuration in force at this landed commit, including commands.
    pub check_set: CheckSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandWork {
    /// Newest landed commit beyond the green prefix; repeated offers are safe.
    pub job: Option<PostLandJob>,
    /// No commit is assumed green before the first successful full run.
    pub last_green: Option<ObjectId>,
}
