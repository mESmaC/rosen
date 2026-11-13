//! Presence: the members joined, their machine and message mode (R13).

use super::{enum_from, messages, now_millis, project_line, ChannelError, Written};
use rosen_protocol::channel::{ChannelAnswer, ChannelEvent, Member, MessageMode};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};

const COLUMNS: &str = "member, nick, machine, mode, joined_at, left_at";

/// Joins or rejoins `member`. A rejoin keeps the member's mode, so a
/// quarantine is not lifted by restarting the agent.
pub fn join(
    tx: &Transaction,
    member: &str,
    nick: &str,
    machine: &str,
) -> Result<Written, ChannelError> {
    for (name, value) in [("member", member), ("nick", nick), ("machine", machine)] {
        if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ChannelError::Invalid(format!("invalid {name} {value:?}")));
        }
    }
    // A nick names one joined member, or a message to it would be ambiguous.
    let holder: Option<String> = tx
        .query_row(
            "SELECT member FROM channel_members
             WHERE nick = ?1 AND member <> ?2 AND left_at IS NULL",
            params![nick, member],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(holder) = holder {
        return Err(ChannelError::NickTaken {
            nick: nick.to_owned(),
            member: holder,
        });
    }
    tx.execute(
        "INSERT INTO channel_members (member, nick, machine, mode, joined_at, left_at)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL)
         ON CONFLICT (member) DO UPDATE SET nick = excluded.nick,
             machine = excluded.machine, joined_at = excluded.joined_at, left_at = NULL",
        params![
            member,
            nick,
            machine,
            MessageMode::Open.as_str(),
            now_millis()
        ],
    )?;
    let member = get(tx, member)?;
    let line = project_line(&ChannelEvent::Joined {
        member: member.clone(),
    })?;
    Ok((ChannelAnswer::Member(member), vec![line]))
}

/// Marks the member gone. Its piece claims and held messages stay.
pub fn leave(tx: &Transaction, member: &str) -> Result<Written, ChannelError> {
    // Leaving twice keeps the first time it left, and posts one line.
    let changed = tx.execute(
        "UPDATE channel_members SET left_at = ?2 WHERE member = ?1 AND left_at IS NULL",
        params![member, now_millis()],
    )?;
    let member = get(tx, member)?;
    let mut lines = Vec::new();
    if changed > 0 {
        lines.push(project_line(&ChannelEvent::Left {
            member: member.clone(),
        })?);
    }
    Ok((ChannelAnswer::Member(member), lines))
}

/// Sets the member's mode. Leaving heads-down for open delivers what it held.
pub fn set_mode(
    tx: &Transaction,
    member: &str,
    mode: MessageMode,
) -> Result<Written, ChannelError> {
    get(tx, member)?;
    tx.execute(
        "UPDATE channel_members SET mode = ?2 WHERE member = ?1",
        params![member, mode.as_str()],
    )?;
    let updated = get(tx, member)?;
    let mut lines = vec![project_line(&ChannelEvent::Mode {
        member: updated.clone(),
    })?];
    if mode == MessageMode::Open {
        lines.extend(messages::deliver_held(tx, &updated)?);
    }
    Ok((ChannelAnswer::Member(updated), lines))
}

/// Every member that ever joined: joined ones first, then by nick.
pub fn list(conn: &Connection) -> Result<Vec<Member>, ChannelError> {
    let sql =
        format!("SELECT {COLUMNS} FROM channel_members ORDER BY left_at IS NOT NULL, nick, member");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], read_member)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The member's row; [`ChannelError::NotJoined`] when it never joined.
pub fn get(conn: &Connection, member: &str) -> Result<Member, ChannelError> {
    let sql = format!("SELECT {COLUMNS} FROM channel_members WHERE member = ?1");
    conn.query_row(&sql, [member], read_member)
        .optional()?
        .ok_or_else(|| ChannelError::NotJoined(member.to_owned()))
}

/// The member a nick names: the joined one, or else the one that used it
/// last, so a message to an agent that just left is still kept for it.
pub fn by_nick(conn: &Connection, nick: &str) -> Result<Member, ChannelError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM channel_members WHERE nick = ?1
         ORDER BY left_at IS NOT NULL, joined_at DESC LIMIT 1"
    );
    conn.query_row(&sql, [nick], read_member)
        .optional()?
        .ok_or_else(|| ChannelError::UnknownNick(nick.to_owned()))
}

/// Every joined member's ID.
pub fn joined(conn: &Connection) -> Result<Vec<Member>, ChannelError> {
    let sql = format!("SELECT {COLUMNS} FROM channel_members WHERE left_at IS NULL");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], read_member)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn read_member(row: &Row) -> rusqlite::Result<Member> {
    Ok(Member {
        member: row.get(0)?,
        nick: row.get(1)?,
        machine: row.get(2)?,
        mode: enum_from(row.get(3)?, MessageMode::parse)?,
        joined_at: row.get(4)?,
        left_at: row.get(5)?,
    })
}
