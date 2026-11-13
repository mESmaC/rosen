//! Enrollment contracts (SPEC R14). Times are Unix seconds.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineRole {
    Agent,
    Checker,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    Active,
    Pending,
    Revoked,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineRecord {
    pub id: String,
    pub name: String,
    pub role: MachineRole,
    pub status: MachineStatus,
    pub last_contact: u64,
    pub running_agents: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinKeyRecord {
    pub id: String,
    pub expires_at: u64,
    pub remaining: u32,
    pub role: MachineRole,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateJoinKey {
    pub address: String,
    pub expires_at: u64,
    pub use_limit: u32,
    pub role: MachineRole,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineCredential {
    pub machine_id: String,
    pub secret: String,
    pub expires_at: u64,
}
impl std::fmt::Debug for MachineCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MachineCredential")
            .field("machine_id", &self.machine_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct JoinKey {
    pub address: String,
    pub fingerprint: [u8; 32],
    pub secret: String,
}
