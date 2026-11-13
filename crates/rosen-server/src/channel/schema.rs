//! The channel's tables in the metadata store (SPEC R13, D13).
//!
//! Rows are authoritative channel state, so they are installed with the core
//! schema in `metadata::ensure_schema`. Every statement is `IF NOT EXISTS`,
//! so existing databases gain the tables without losing a row.

use rusqlite::Connection;

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channel_members (
             member    TEXT    PRIMARY KEY,
             nick      TEXT    NOT NULL,
             machine   TEXT    NOT NULL,
             mode      TEXT    NOT NULL
                 CHECK (mode IN ('open', 'heads_down', 'quarantined')),
             joined_at INTEGER NOT NULL,
             left_at   INTEGER
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS channel_members_nick ON channel_members (nick);
         -- One row per piece: the primary key is the first-claim-wins lock.
         CREATE TABLE IF NOT EXISTS channel_pieces (
             piece  TEXT    PRIMARY KEY,
             member TEXT    NOT NULL REFERENCES channel_members (member),
             task   TEXT    NOT NULL,
             branch TEXT    NOT NULL,
             since  INTEGER NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS channel_pieces_member ON channel_pieces (member);
         CREATE TABLE IF NOT EXISTS channel_messages (
             seq        INTEGER PRIMARY KEY,
             from_nick  TEXT    NOT NULL,
             member     TEXT,
             to_nicks   TEXT    NOT NULL,
             intent     TEXT    NOT NULL,
             text       TEXT    NOT NULL,
             created_at INTEGER NOT NULL
         );
         -- AUTOINCREMENT: a reader's cursor counts delivery ids, and a held
         -- delivery is re-inserted on release, so ids must never be reused.
         CREATE TABLE IF NOT EXISTS channel_deliveries (
             id     INTEGER PRIMARY KEY AUTOINCREMENT,
             seq    INTEGER NOT NULL REFERENCES channel_messages (seq),
             member TEXT    NOT NULL,
             held   INTEGER NOT NULL CHECK (held IN (0, 1)),
             UNIQUE (seq, member)
         );
         CREATE INDEX IF NOT EXISTS channel_deliveries_member
             ON channel_deliveries (member, held, id);
         CREATE TABLE IF NOT EXISTS channel_ledger (
             seq            INTEGER PRIMARY KEY,
             ledger         TEXT    NOT NULL CHECK (ledger IN ('discovery', 'past_work')),
             nick           TEXT    NOT NULL,
             member         TEXT,
             text           TEXT    NOT NULL,
             velocity_issue TEXT,
             created_at     INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS channel_ledger_kind ON channel_ledger (ledger, seq);",
    )
}
