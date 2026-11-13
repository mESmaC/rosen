//! Piece claims: first-claim-wins locks on work items (R13).
//!
//! The rule is `rosen_core::piece::decide`, run between the read of the
//! piece's row and the write, inside one IMMEDIATE transaction. The table's
//! primary key backs it: even a second writer that skipped the read could
//! not store a second holder.

use super::{presence, project_line, ChannelError, Written};
use rosen_core::{decide_piece, piece_key, PieceDecision};
use rosen_protocol::channel::{ChannelAnswer, ChannelEvent, MessageMode, PieceClaim, PieceOutcome};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

const COLUMNS: &str = "p.piece, p.member, m.nick, p.task, p.branch, p.since";
const FROM: &str = "channel_pieces p JOIN channel_members m ON m.member = p.member";

/// Claims a piece for `member`, extends its own claim, or names the holder.
pub fn claim(
    tx: &Transaction,
    member: &str,
    piece: Option<&str>,
    task: &str,
    branch: &str,
) -> Result<Written, ChannelError> {
    let key = piece_key(piece, task).ok_or_else(|| {
        ChannelError::Invalid("a piece claim needs a piece identity or task text".into())
    })?;
    let claimant = unfrozen(tx, member)?;
    let holder = get(tx, &key)?;
    let extended = match decide_piece(holder.as_ref().map(|h| h.member.as_str()), member) {
        PieceDecision::Refuse => {
            let holder = holder.expect("refused only when held");
            let event = ChannelEvent::Refused {
                nick: claimant.nick,
                holder: holder.clone(),
            };
            let outcome = PieceOutcome::Refused { holder };
            return Ok((ChannelAnswer::Piece(outcome), vec![project_line(&event)?]));
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
    let claim = get(tx, &key)?.expect("stored above");
    let event = ChannelEvent::Claim {
        claim: claim.clone(),
        extended,
    };
    let outcome = PieceOutcome::Granted { claim, extended };
    Ok((ChannelAnswer::Piece(outcome), vec![project_line(&event)?]))
}

/// Releases `member`'s claim on one piece, or all of them. Answers with what
/// was released; a piece another member holds is never touched.
pub fn release(
    tx: &Transaction,
    member: &str,
    piece: Option<&str>,
) -> Result<Written, ChannelError> {
    unfrozen(tx, member)?;
    let held: Vec<PieceClaim> = held_by(tx, member)?
        .into_iter()
        .filter(|claim| piece.is_none_or(|key| claim.piece == key.trim()))
        .collect();
    for claim in &held {
        tx.execute(
            "DELETE FROM channel_pieces WHERE piece = ?1 AND member = ?2",
            params![claim.piece, member],
        )?;
    }
    let mut lines = Vec::new();
    if !held.is_empty() {
        lines.push(project_line(&ChannelEvent::Released {
            pieces: held.clone(),
        })?);
    }
    Ok((ChannelAnswer::Pieces(held), lines))
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

/// The member, unless it is quarantined: its claims are then frozen
/// (Velocity F14), so it can neither take, extend nor release one.
fn unfrozen(
    conn: &Connection,
    member: &str,
) -> Result<rosen_protocol::channel::Member, ChannelError> {
    let found = presence::get(conn, member)?;
    match found.mode {
        MessageMode::Quarantined => Err(ChannelError::Frozen(member.to_owned())),
        _ => Ok(found),
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
