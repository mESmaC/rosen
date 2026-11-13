//! Agent messages and the user's comments, with their `to` and `intent`
//! (R13, Velocity F14).
//!
//! Each message is stored once; who it reaches is decided when it is sent
//! and stored as one delivery row per recipient, so a later mode change
//! never rewrites who an earlier message was for. A heads-down recipient's
//! delivery is stored held, and released later under a new delivery id, so
//! a reader paging by delivery id still reads it.

use super::{
    enum_from, from_seq, line, now_millis, presence, to_seq, ChannelError, Written, PAGE_LIMIT,
};
use crate::events::ChannelLine;
use rosen_protocol::channel::{
    Author, ChannelAnswer, ChannelEvent, ChannelMessage, Intent, Member, MessageMode, MessagePage,
    SendMessage,
};
use rusqlite::{params, Connection, Row, Transaction};

/// The nick the user's comments carry (Velocity F14).
pub const USER_NICK: &str = "you";

const COLUMNS: &str = "m.seq, m.from_nick, m.member, m.to_nicks, m.intent, m.text, m.created_at";

/// Stores a message and its deliveries, and posts it to every recipient
/// whose delivery is not held.
pub fn send(tx: &Transaction, message: &SendMessage) -> Result<Written, ChannelError> {
    if message.text.trim().is_empty() {
        return Err(ChannelError::Invalid("a message needs text".into()));
    }
    let sender = match &message.from {
        Author::Agent { member } => Some(presence::get(tx, member)?),
        Author::User => None,
    };
    if sender.is_some() && message.to.is_empty() {
        return Err(ChannelError::Invalid(
            "an agent's message must name its recipients; agents cannot broadcast".into(),
        ));
    }
    let recipients = recipients(tx, sender.as_ref(), &message.to)?;
    let to =
        serde_json::to_string(&message.to).map_err(|e| ChannelError::Invalid(e.to_string()))?;
    tx.execute(
        "INSERT INTO channel_messages (from_nick, member, to_nicks, intent, text, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            sender.as_ref().map_or(USER_NICK, |s| s.nick.as_str()),
            sender.as_ref().map(|s| s.member.as_str()),
            to,
            message.intent.as_str(),
            message.text,
            now_millis()
        ],
    )?;
    let seq = tx.last_insert_rowid();
    for (recipient, held) in &recipients {
        tx.execute(
            "INSERT OR IGNORE INTO channel_deliveries (seq, member, held) VALUES (?1, ?2, ?3)",
            params![seq, recipient.member, held],
        )?;
    }
    let sent = get(tx, seq)?;
    let now: Vec<&str> = recipients
        .iter()
        .filter(|(_, held)| !held)
        .map(|(recipient, _)| recipient.nick.as_str())
        .collect();
    let event = ChannelEvent::Message {
        message: sent.clone(),
    };
    Ok((ChannelAnswer::Sent(sent), vec![line(&event, &now)?]))
}

/// Who a message reaches, and whether each delivery is held:
/// - a quarantined sender's message reaches nobody;
/// - the user's comments reach every named member, or every joined member
///   when none is named, and are never held;
/// - another agent's message skips quarantined recipients and the sender
///   itself, and is held for heads-down ones.
fn recipients(
    tx: &Transaction,
    sender: Option<&Member>,
    to: &[String],
) -> Result<Vec<(Member, bool)>, ChannelError> {
    if sender.is_some_and(|s| s.mode == MessageMode::Quarantined) {
        return Ok(Vec::new());
    }
    let named = if to.is_empty() {
        presence::joined(tx)?
    } else {
        // Resolve every nick before storing anything: one unknown nick
        // refuses the whole message rather than reaching only some.
        to.iter()
            .map(|nick| presence::by_nick(tx, nick))
            .collect::<Result<Vec<_>, _>>()?
    };
    let Some(sender) = sender else {
        return Ok(named.into_iter().map(|m| (m, false)).collect());
    };
    Ok(named
        .into_iter()
        .filter(|m| m.member != sender.member && m.mode != MessageMode::Quarantined)
        .map(|m| {
            let held = m.mode == MessageMode::HeadsDown;
            (m, held)
        })
        .collect())
}

/// Messages after `after`: those delivered to `reader`, or every message.
pub fn read(
    conn: &Connection,
    reader: Option<&str>,
    after: u64,
) -> Result<MessagePage, ChannelError> {
    let limit = from_seq(PAGE_LIMIT);
    let rows: Vec<(u64, i64)> = match reader {
        Some(member) => {
            presence::get(conn, member)?;
            let mut stmt = conn.prepare_cached(
                "SELECT id, seq FROM channel_deliveries
                 WHERE member = ?1 AND held = 0 AND id > ?2 ORDER BY id LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![member, from_seq(after), limit], |row| {
                Ok((to_seq(row.get(0)?), row.get(1)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        }
        None => {
            let mut stmt = conn.prepare_cached(
                "SELECT seq FROM channel_messages WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![from_seq(after), limit], |row| {
                let seq: i64 = row.get(0)?;
                Ok((to_seq(seq), seq))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        }
    };
    let cursor = rows.last().map_or(after, |(cursor, _)| *cursor);
    let messages = rows
        .iter()
        .map(|(_, seq)| get(conn, *seq))
        .collect::<Result<_, _>>()?;
    Ok(MessagePage { messages, cursor })
}

/// Delivers every message held for `member`, answering how many.
pub fn release_held(tx: &Transaction, member: &str) -> Result<Written, ChannelError> {
    let member = presence::get(tx, member)?;
    let lines = deliver_held(tx, &member)?;
    Ok((ChannelAnswer::Delivered(lines.len() as u64), lines))
}

/// Re-inserts each held delivery as delivered, in message order, under new
/// ids past every reader cursor, and posts each message to the member.
///
/// A quarantined member gets nothing from other agents (R13), so its held
/// deliveries stay held, to be delivered once it returns to open mode.
pub(super) fn deliver_held(
    tx: &Transaction,
    member: &Member,
) -> Result<Vec<ChannelLine>, ChannelError> {
    if member.mode == MessageMode::Quarantined {
        return Ok(Vec::new());
    }
    let held: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT seq FROM channel_deliveries WHERE member = ?1 AND held = 1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([&member.member], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut lines = Vec::with_capacity(held.len());
    for seq in held {
        tx.execute(
            "DELETE FROM channel_deliveries WHERE seq = ?1 AND member = ?2",
            params![seq, member.member],
        )?;
        tx.execute(
            "INSERT INTO channel_deliveries (seq, member, held) VALUES (?1, ?2, 0)",
            params![seq, member.member],
        )?;
        let event = ChannelEvent::Message {
            message: get(tx, seq)?,
        };
        lines.push(line(&event, &[member.nick.as_str()])?);
    }
    Ok(lines)
}

fn get(conn: &Connection, seq: i64) -> Result<ChannelMessage, ChannelError> {
    let sql = format!("SELECT {COLUMNS} FROM channel_messages m WHERE m.seq = ?1");
    let mut message = conn.query_row(&sql, [seq], read_message)?;
    let mut stmt = conn
        .prepare_cached("SELECT member FROM channel_deliveries WHERE seq = ?1 ORDER BY member")?;
    let rows = stmt.query_map([seq], |row| row.get(0))?;
    message.recipients = rows.collect::<rusqlite::Result<_>>()?;
    Ok(message)
}

fn read_message(row: &Row) -> rusqlite::Result<ChannelMessage> {
    let to: String = row.get(3)?;
    Ok(ChannelMessage {
        seq: to_seq(row.get(0)?),
        from: row.get(1)?,
        member: row.get(2)?,
        to: serde_json::from_str(&to).unwrap_or_default(),
        intent: enum_from(row.get(4)?, Intent::parse)?,
        text: row.get(5)?,
        time: row.get(6)?,
        recipients: Vec::new(),
    })
}
