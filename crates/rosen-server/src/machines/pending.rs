//! Bounds on pending admission (R14, #155). A pending request needs no
//! credential, so any peer that reaches the listener can make one; without
//! these bounds it could grow metadata without limit and flood the owner's
//! approval list.
use rusqlite::{params, Connection};
use std::net::IpAddr;

/// An unapproved request is removed this many seconds after it was made.
pub const EXPIRY: u64 = 7 * 24 * 3600;
/// Requests that may await approval at once, from all sources together.
pub const MAX_WAITING: u64 = 64;
/// Requests that may await approval at once from one source.
pub const MAX_WAITING_PER_SOURCE: u64 = 4;
/// The source recorded for a request over the local transport.
pub const LOCAL_SOURCE: &str = "local";

/// The source a network peer's requests count against. An IPv6 peer is
/// counted by its /64, since one host can usually take any address in it.
pub fn source_of(peer: IpAddr) -> String {
    match peer {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
    }
}

/// Adds the column recording where a pending request came from to a
/// database created before it existed.
pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    if has_source_column(conn)? {
        return Ok(());
    }
    match conn.execute(
        "ALTER TABLE machines ADD COLUMN source TEXT NOT NULL DEFAULT ''",
        [],
    ) {
        Ok(_) => Ok(()),
        // Another connection opening the database at the same moment may
        // have added it first.
        Err(_) if has_source_column(conn)? => Ok(()),
        Err(err) => Err(err),
    }
}

fn has_source_column(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('machines') WHERE name = 'source'",
        [],
        |r| r.get(0),
    )
}

/// Removes unapproved requests older than [`EXPIRY`]. A pending machine
/// cannot authenticate, so its last contact stays the time it asked, and it
/// has no receipt key bound yet: a key is bound on an active machine's first
/// signed receipt.
pub fn expire(conn: &Connection, now: u64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM machines WHERE status='pending' AND last_contact<=?1",
        params![now.saturating_sub(EXPIRY)],
    )?;
    Ok(())
}

/// Expires stale requests, then refuses a new one from `source` while the
/// approval list or that source's share of it is full. Run in the same
/// transaction as the insert, so concurrent requests cannot pass a cap.
pub fn admit(conn: &Connection, source: &str, now: u64) -> Result<(), String> {
    expire(conn, now).map_err(|e| e.to_string())?;
    let (waiting, from_source): (u64, u64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(source=?1),0) FROM machines WHERE status='pending'",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    if waiting >= MAX_WAITING {
        return Err(format!(
            "{waiting} machines already await approval; the owner must approve or \
             revoke some first, or join with a key"
        ));
    }
    if from_source >= MAX_WAITING_PER_SOURCE {
        return Err(format!(
            "{from_source} machines from this address already await approval; \
             the owner must approve or revoke them first, or join with a key"
        ));
    }
    Ok(())
}
