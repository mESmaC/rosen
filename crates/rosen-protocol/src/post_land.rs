//! External build and full-test checker interface (SPEC R12).
use rosen_core::{project_config::CheckSet, ObjectId};
use serde::{Deserialize, Serialize};

/// Build includes lint; tests may fail without making an after-land base unbuildable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckKind {
    Build,
    #[default]
    Test,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Test => "test",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandJob {
    pub commit: ObjectId,
    #[serde(default)]
    pub kind: CheckKind,
    /// Configuration in force at this landed commit, including commands.
    pub check_set: CheckSet,
}

/// A confirmed failure stays attached to its exact land even after repair or revert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandFailure {
    pub session: i64,
    pub change: String,
    pub commit: ObjectId,
    pub kind: CheckKind,
    pub output: String,
    pub requires_revert: bool,
    pub revert: Option<ObjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLandWork {
    /// Newest landed commit beyond the green prefix; repeated offers are safe.
    pub job: Option<PostLandJob>,
    /// No commit is assumed green before the first successful full run.
    pub last_green: Option<ObjectId>,
}
