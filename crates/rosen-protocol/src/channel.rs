//! Wire shapes for the project's AIRC channel (SPEC R13): presence, piece
//! claims, agent messages and the Discoveries and Past work ledgers.
//!
//! Velocity serves AIRC's tools and calls these for each one, with the
//! member ID and nick it fixed at the agent's launch. File claims, shares and
//! waits are Rosen path claims (R6) and use the claim requests instead.

use serde::{Deserialize, Serialize};

/// One channel operation, carried by [`crate::Request::Channel`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ChannelRequest {
    /// Joins `member` under `nick` from `machine`, or rejoins it. A rejoin
    /// keeps its message mode, so a quarantine survives a restart.
    Join {
        member: String,
        nick: String,
        machine: String,
    },
    /// Marks the member as gone. Its piece claims stay (Velocity F14: a
    /// failed or closed thread keeps holding its claim).
    Leave {
        member: String,
    },
    SetMode {
        member: String,
        mode: MessageMode,
    },
    /// Every member that ever joined, joined ones first.
    Presence,
    /// First claim wins. Without `piece`, the task text is the lock.
    ClaimPiece {
        member: String,
        #[serde(default)]
        piece: Option<String>,
        task: String,
        branch: String,
    },
    /// Releases the member's claim on one piece, or on all of its pieces
    /// when `piece` is `None`. Pieces another member holds are left alone.
    ReleasePiece {
        member: String,
        #[serde(default)]
        piece: Option<String>,
    },
    /// Every held piece, oldest claim first.
    Pieces,
    Send(SendMessage),
    /// Messages after `after`. With a `reader`, only those delivered to that
    /// member; without one, every message (the channel view).
    Messages {
        #[serde(default)]
        reader: Option<String>,
        after: u64,
    },
    /// Delivers the messages a heads-down member's mode held back. Velocity
    /// calls it when that agent opens or updates its PR, or goes idle.
    ReleaseHeld {
        member: String,
    },
    Append(AppendEntry),
    /// Ledger entries after `after`, oldest first.
    ReadLedger {
        ledger: Ledger,
        after: u64,
    },
}

/// Who wrote a message or ledger entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Author {
    /// A joined member, by member ID.
    Agent { member: String },
    /// The project's user (nick `you`), whose comments reach every agent.
    User,
}

/// Parameters of [`ChannelRequest::Send`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessage {
    pub from: Author,
    /// Recipient nicks. Agents must name at least one; only the user's
    /// comments may name none, and then go to every joined member.
    pub to: Vec<String>,
    pub intent: Intent,
    pub text: String,
}

/// Why a message was sent (Velocity F14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Inform,
    Request,
    Question,
}

/// How much of the channel reaches a member (Velocity F14).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageMode {
    /// Receives what targeted delivery sends it.
    #[default]
    Open,
    /// Other agents' messages are held until [`ChannelRequest::ReleaseHeld`]
    /// or a switch back to open. The user's comments always get through.
    HeadsDown,
    /// Receives nothing from other agents, its messages reach nobody, and
    /// its piece claims are frozen: not taken, extended or released.
    Quarantined,
}

/// The two channel ledgers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ledger {
    Discovery,
    PastWork,
}

/// Parameters of [`ChannelRequest::Append`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendEntry {
    pub ledger: Ledger,
    pub author: Author,
    pub text: String,
    /// For a discovery about Velocity itself: the issue title Velocity files.
    #[serde(default)]
    pub velocity_issue: Option<String>,
}

/// A member's presence row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub member: String,
    pub nick: String,
    pub machine: String,
    pub mode: MessageMode,
    /// Unix milliseconds of its latest join.
    pub joined_at: i64,
    /// Unix milliseconds it left, while it is gone.
    pub left_at: Option<i64>,
}

/// One held piece.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceClaim {
    /// The lock key: the piece identity, or the task text without one.
    pub piece: String,
    pub member: String,
    pub nick: String,
    pub task: String,
    pub branch: String,
    /// Unix milliseconds of the first claim. An extension keeps it.
    pub since: i64,
}

/// The answer to [`ChannelRequest::ClaimPiece`]. A refusal is an ordinary
/// answer, not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PieceOutcome {
    Granted {
        claim: PieceClaim,
        /// True when the claimant already held the piece.
        extended: bool,
    },
    /// Another member holds it since `holder.since`.
    Refused { holder: PieceClaim },
}

/// One stored message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMessage {
    pub seq: u64,
    /// The sender's nick (`you` for the user).
    pub from: String,
    /// The sender's member ID; `None` for the user.
    pub member: Option<String>,
    /// The recipient nicks as sent.
    pub to: Vec<String>,
    pub intent: Intent,
    pub text: String,
    /// Unix milliseconds when it was stored.
    pub time: i64,
    /// Member IDs it was delivered to, or held for, at send time. Empty for a
    /// quarantined sender's message, which reaches nobody.
    pub recipients: Vec<String>,
}

/// The answer to [`ChannelRequest::Messages`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePage {
    pub messages: Vec<ChannelMessage>,
    /// Pass as `after` next time. For a reader it counts deliveries, not
    /// message numbers, so a held message released later is still read.
    pub cursor: u64,
}

/// One ledger entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub seq: u64,
    pub ledger: Ledger,
    pub nick: String,
    pub member: Option<String>,
    pub text: String,
    pub velocity_issue: Option<String>,
    pub time: i64,
}

/// The stored and wire spelling of the channel's unit enums, so the server
/// can keep them in SQLite text columns.
macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some($name::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

text_enum!(Intent { Inform => "inform", Request => "request", Question => "question" });
text_enum!(MessageMode {
    Open => "open",
    HeadsDown => "heads_down",
    Quarantined => "quarantined",
});
text_enum!(Ledger { Discovery => "discovery", PastWork => "past_work" });

/// Successful channel results, carried by [`crate::Response::Channel`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub enum ChannelAnswer {
    /// The answer to `join`, `leave` and `set_mode`.
    Member(Member),
    Members(Vec<Member>),
    Piece(PieceOutcome),
    /// The answer to `pieces`, and to `release_piece` (the pieces released).
    Pieces(Vec<PieceClaim>),
    Sent(ChannelMessage),
    Messages(MessagePage),
    /// How many held messages `release_held` delivered.
    Delivered(u64),
    Entry(LedgerEntry),
    Entries(Vec<LedgerEntry>),
}
