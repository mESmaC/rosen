use super::{keys, now, open};
use rosen_protocol::MachineCredential;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use std::path::Path;
pub const LIFETIME: u64 = 30 * 24 * 3600;
fn fresh(id: String) -> Result<MachineCredential, String> {
    Ok(MachineCredential {
        machine_id: id,
        secret: keys::secret()?,
        expires_at: now() + LIFETIME,
    })
}
pub fn enroll(dir: &Path, key: &str, name: &str) -> Result<MachineCredential, String> {
    if name.is_empty() || name.len() > 256 {
        return Err("machine name must contain 1..256 bytes".into());
    }
    let mut conn = open(dir).map_err(|e| e.to_string())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let hash = keys::digest(key);
    let role: String = tx
        .query_row(
            "SELECT role FROM join_keys WHERE secret_hash=?1 AND expires_at>?2 AND remaining>0",
            params![hash, now()],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or("join key expired, exhausted or revoked")?;
    let credential = fresh(keys::secret()?)?;
    tx.execute(
        "UPDATE join_keys SET remaining=remaining-1 WHERE secret_hash=?1",
        [&hash],
    )
    .map_err(|e| e.to_string())?;
    tx.execute("INSERT INTO machines(id,name,role,status,last_contact,credential_hash,expires_at) VALUES (?1,?2,?3,'active',?4,?5,?6)", params![credential.machine_id,name,role,now(),keys::digest(&credential.secret),credential.expires_at]).map_err(|e|e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(credential)
}
pub fn rotate(dir: &Path, secret: &str) -> Result<MachineCredential, String> {
    let mut conn = open(dir).map_err(|e| e.to_string())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let hash = keys::digest(secret);
    let retry: Option<(String,u64)> = tx.query_row("SELECT id,expires_at FROM machines WHERE previous_hash=?1 AND status='active' AND previous_expires>?2", params![hash,now()], |r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|e.to_string())?;
    if let Some((machine_id, expires_at)) = retry {
        return Ok(MachineCredential {
            machine_id,
            expires_at,
            secret: rotated_secret(dir, secret)?,
        });
    }
    let id: String = tx.query_row("SELECT id FROM machines WHERE credential_hash=?1 AND status='active' AND expires_at>?2", params![hash,now()], |r|r.get(0)).optional().map_err(|e|e.to_string())?.ok_or("machine credential expired or revoked")?;
    let mut credential = fresh(id)?;
    // Derivation makes a lost response retryable without retaining plaintext secrets.
    credential.secret = rotated_secret(dir, secret)?;
    tx.execute("UPDATE machines SET previous_hash=credential_hash,previous_expires=expires_at,credential_hash=?1,expires_at=?2 WHERE id=?3", params![keys::digest(&credential.secret),credential.expires_at,credential.machine_id]).map_err(|e|e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(credential)
}

pub fn pending(
    dir: &Path,
    name: &str,
    role: rosen_protocol::MachineRole,
) -> Result<MachineCredential, String> {
    if name.is_empty() || name.len() > 256 {
        return Err("machine name must contain 1..256 bytes".into());
    }
    let conn = open(dir).map_err(|e| e.to_string())?;
    let credential = fresh(keys::secret()?)?;
    let role = match role {
        rosen_protocol::MachineRole::Agent => "agent",
        rosen_protocol::MachineRole::Checker => "checker",
    };
    conn.execute("INSERT INTO machines(id,name,role,status,last_contact,credential_hash,expires_at) VALUES (?1,?2,?3,'pending',?4,?5,?6)",params![credential.machine_id,name,role,now(),keys::digest(&credential.secret),credential.expires_at]).map_err(|e|e.to_string())?;
    Ok(credential)
}

fn rotated_secret(dir: &Path, prior: &str) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let identity = crate::network::Identity::load_or_create(dir).map_err(|e| e.to_string())?;
    // The server key makes retryable renewal unpredictable to a holder of an old credential.
    let mut hash = Sha256::new();
    hash.update(b"rosen-credential-rotation-v1");
    hash.update(identity.private_key());
    hash.update(prior.as_bytes());
    Ok(crate::network::identity::hex(&hash.finalize()))
}
