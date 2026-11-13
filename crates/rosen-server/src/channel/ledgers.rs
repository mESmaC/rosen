//! The Discoveries and Past work ledgers (R13): append-only entries.

use super::{
    enum_from, from_seq, messages::USER_NICK, now_millis, presence, to_seq, ChannelError,
    PAGE_LIMIT,
};
use rosen_protocol::channel::{AppendEntry, Author, Ledger, LedgerEntry};
use rusqlite::{params, Connection, Row};

const COLUMNS: &str = "seq, ledger, nick, member, text, velocity_issue, created_at";

pub fn append(conn: &mut Connection, entry: &AppendEntry) -> Result<LedgerEntry, ChannelError> {
    if entry.text.trim().is_empty() {
        return Err(ChannelError::Invalid("a ledger entry needs text".into()));
    }
    let tx = super::write_tx(conn)?;
    let (nick, member) = match &entry.author {
        Author::Agent { member } => {
            let author = presence::get(&tx, member)?;
            (author.nick, Some(author.member))
        }
        Author::User => (USER_NICK.to_owned(), None),
    };
    tx.execute(
        "INSERT INTO channel_ledger (ledger, nick, member, text, velocity_issue, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            entry.ledger.as_str(),
            nick,
            member,
            entry.text,
            entry.velocity_issue,
            now_millis()
        ],
    )?;
    let seq = tx.last_insert_rowid();
    let sql = format!("SELECT {COLUMNS} FROM channel_ledger WHERE seq = ?1");
    let stored = tx.query_row(&sql, [seq], read_entry)?;
    tx.commit()?;
    Ok(stored)
}

/// Entries of one ledger after `after`, oldest first.
pub fn read(
    conn: &Connection,
    ledger: Ledger,
    after: u64,
) -> Result<Vec<LedgerEntry>, ChannelError> {
    let sql = format!(
        "SELECT {COLUMNS} FROM channel_ledger WHERE ledger = ?1 AND seq > ?2
         ORDER BY seq LIMIT ?3"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(
        params![ledger.as_str(), from_seq(after), from_seq(PAGE_LIMIT)],
        read_entry,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn read_entry(row: &Row) -> rusqlite::Result<LedgerEntry> {
    Ok(LedgerEntry {
        seq: to_seq(row.get(0)?),
        ledger: enum_from(row.get(1)?, Ledger::parse)?,
        nick: row.get(2)?,
        member: row.get(3)?,
        text: row.get(4)?,
        velocity_issue: row.get(5)?,
        time: row.get(6)?,
    })
}
