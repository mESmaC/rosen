//! Piece claims: first-claim-wins locks on work items (R13).
//!
//! The rule is `rosen_core::piece::decide`, run between the read of the
//! piece's row and the write, inside one IMMEDIATE transaction. The table's
//! primary key backs it: even a second writer that skipped the read could
//! not store a second holder.

use super::{presence, write_tx, ChannelError};
use rosen_core::{decide_piece, piece_key, PieceDecision};
use rosen_protocol::channel::{MessageMode, PieceClaim, PieceOutcome};
use rusqlite::{params, Connection, OptionalExtension, Row};

const COLUMNS: &str = "p.piece, p.member, m.nick, p.task, p.branch, p.since";
const FROM: &str = "channel_pieces p JOIN channel_members m ON m.member = p.member";

/// Claims a piece for `member`, extends its own claim, or names the holder.
pub fn claim(
    conn: &mut Connection,
    member: &str,
    piece: Option<&str>,
    task: &str,
    branch: &str,
) -> Result<PieceOutcome, ChannelError> {
    let key = piece_key(piece, task).ok_or_else(|| {
        ChannelError::Invalid("a piece claim needs a piece identity or task text".into())
    })?;
    let tx = write_tx(conn)?;
    ensure_unfrozen(&tx, member)?;
    let holder = get(&tx, &key)?;
    let outcome = match decide_piece(holder.as_ref().map(|h| h.member.as_str()), member) {
        PieceDecision::Refuse => {
            let holder = holder.expect("refused only when held");
            return Ok(PieceOutcome::Refused { holder });
        }
        PieceDecision::Extend => {
            tx.execute(
                "UPDATE channel_pieces SET task = ?2, branch = ?3 WHERE piece = ?1",
                params![key, task, branch],
            )?;
            true
        }
        PieceDecision::Grant => {
            tx.execute(
                "INSERT INTO channel_pieces (piece, member, task, branch, since)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key, member, task, branch, super::now_millis()],
            )?;
            false
        }
    };
    let claim = get(&tx, &key)?.expect("stored above");
    tx.commit()?;
    Ok(PieceOutcome::Granted {
        claim,
        extended: outcome,
    })
}

/// Releases `member`'s claim on one piece, or all of them. Returns what was
/// released; a piece another member holds is never touched.
pub fn release(
    conn: &mut Connection,
    member: &str,
    piece: Option<&str>,
) -> Result<Vec<PieceClaim>, ChannelError> {
    let tx = write_tx(conn)?;
    ensure_unfrozen(&tx, member)?;
    let held: Vec<PieceClaim> = held_by(&tx, member)?
        .into_iter()
        .filter(|claim| piece.is_none_or(|key| claim.piece == key.trim()))
        .collect();
    for claim in &held {
        tx.execute(
            "DELETE FROM channel_pieces WHERE piece = ?1 AND member = ?2",
            params![claim.piece, member],
        )?;
    }
    tx.commit()?;
    Ok(held)
}

/// Every held piece, oldest claim first.
pub fn list(conn: &Connection) -> Result<Vec<PieceClaim>, ChannelError> {
    let sql = format!("SELECT {COLUMNS} FROM {FROM} ORDER BY p.since, p.piece");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], read_claim)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn held_by(conn: &Connection, member: &str) -> Result<Vec<PieceClaim>, ChannelError> {
    let sql = format!("SELECT {COLUMNS} FROM {FROM} WHERE p.member = ?1 ORDER BY p.since");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([member], read_claim)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn get(conn: &Connection, key: &str) -> Result<Option<PieceClaim>, ChannelError> {
    let sql = format!("SELECT {COLUMNS} FROM {FROM} WHERE p.piece = ?1");
    Ok(conn.query_row(&sql, [key], read_claim).optional()?)
}

/// A quarantined member's claims are frozen (Velocity F14): it can neither
/// take, extend nor release one until it is let out.
fn ensure_unfrozen(conn: &Connection, member: &str) -> Result<(), ChannelError> {
    match presence::get(conn, member)?.mode {
        MessageMode::Quarantined => Err(ChannelError::Frozen(member.to_owned())),
        _ => Ok(()),
    }
}

fn read_claim(row: &Row) -> rusqlite::Result<PieceClaim> {
    Ok(PieceClaim {
        piece: row.get(0)?,
        member: row.get(1)?,
        nick: row.get(2)?,
        task: row.get(3)?,
        branch: row.get(4)?,
        since: row.get(5)?,
    })
}
