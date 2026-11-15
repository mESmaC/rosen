use super::{keys, now, open, signing};
use rosen_protocol::MachineCredential;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
pub const LIFETIME: u64 = 30 * 24 * 3600;
fn fresh(id: String) -> Result<MachineCredential, String> {
    Ok(MachineCredential {
        machine_id: id,
        secret: keys::secret()?,
        expires_at: now() + LIFETIME,
    })
}
/// Refuses a bad name or a malformed receipt key before anything is written.
fn check_request(name: &str, receipt_key: Option<&str>) -> Result<(), String> {
    if name.is_empty() || name.len() > 256 {
        return Err("machine name must contain 1..256 bytes".into());
    }
    if receipt_key.is_some_and(|key| !signing::is_public_key(key)) {
        return Err("receipt key must be an Ed25519 public key in lowercase hex".into());
    }
    Ok(())
}
/// Binds the daemon's receipt key with the credential it is issued, so no
/// one holding that credential later can bind another (R8 step 1, R14). A
/// request without one leaves the machine to first-use binding.
fn bind_receipt_key(
    conn: &Connection,
    machine: &str,
    receipt_key: Option<&str>,
) -> Result<(), String> {
    let Some(public_key) = receipt_key else {
        return Ok(());
    };
    conn.execute(
        "INSERT INTO machine_signing_keys (machine, public_key) VALUES (?1, ?2)",
        params![machine, public_key],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
pub fn enroll(
    dir: &Path,
    key: &str,
    name: &str,
    receipt_key: Option<&str>,
) -> Result<MachineCredential, String> {
    check_request(name, receipt_key)?;
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
    bind_receipt_key(&tx, &credential.machine_id, receipt_key)?;
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
    source: &str,
    receipt_key: Option<&str>,
) -> Result<MachineCredential, String> {
    check_request(name, receipt_key)?;
    let mut conn = open(dir).map_err(|e| e.to_string())?;
    // Immediate, so the cap check and the insert see no concurrent insert.
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    super::pending::admit(&tx, source, now())?;
    let credential = fresh(keys::secret()?)?;
    let role = match role {
        rosen_protocol::MachineRole::Agent => "agent",
        rosen_protocol::MachineRole::Checker => "checker",
    };
    tx.execute("INSERT INTO machines(id,name,role,status,last_contact,credential_hash,expires_at,source) VALUES (?1,?2,?3,'pending',?4,?5,?6,?7)",params![credential.machine_id,name,role,now(),keys::digest(&credential.secret),credential.expires_at,source]).map_err(|e|e.to_string())?;
    bind_receipt_key(&tx, &credential.machine_id, receipt_key)?;
    tx.commit().map_err(|e| e.to_string())?;
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
