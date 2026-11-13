use super::{now, open};
use crate::ServerState;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rosen_protocol::{CreateJoinKey, JoinKey};
use sha2::{Digest, Sha256};
pub fn digest(secret: &str) -> String {
    crate::network::identity::hex(&Sha256::digest(secret.as_bytes()))
}
pub fn secret() -> Result<String, String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
pub fn create(state: &ServerState, req: CreateJoinKey) -> Result<String, String> {
    if req.expires_at <= now() || req.use_limit == 0 || req.expires_at > i64::MAX as u64 {
        return Err("join key needs a future expiry and positive use limit".into());
    }
    let (_, pin) = state
        .network_status()
        .ok_or("network listener is not enabled")?;
    let key = JoinKey {
        address: req.address,
        fingerprint: crate::network::identity::parse_fingerprint(&pin)?,
        secret: secret()?,
    };
    let conn = open(&state.config.data_dir).map_err(|e| e.to_string())?;
    let hash = digest(&key.secret);
    let role = match req.role {
        rosen_protocol::MachineRole::Agent => "agent",
        rosen_protocol::MachineRole::Checker => "checker",
    };
    conn.execute(
        "INSERT INTO join_keys VALUES (?1,?1,?2,?3,?4)",
        rusqlite::params![hash, req.expires_at, req.use_limit, role],
    )
    .map_err(|e| e.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&key).map_err(|e| e.to_string())?))
}
