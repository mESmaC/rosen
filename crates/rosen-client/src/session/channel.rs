//! The project's AIRC channel through a session (SPEC.md R13, §9 "AIRC").
//!
//! Velocity fixes each agent's member ID and nick at launch and serves the
//! AIRC tools by calling the server for every one. [`Session::join_channel`]
//! fixes both once, as the session fixes the agent, so every tool call is
//! made as that member: Velocity keeps no channel state and no local AIRC
//! files, only the delivery cursor this handle carries.

use super::Session;
use crate::Error;
use rosen_protocol::channel::{
    AppendEntry, Author, ChannelMessage, Intent, Ledger, LedgerEntry, Member, MessageMode,
    MessagePage, PieceClaim, PieceOutcome, SendMessage,
};
use rosen_protocol::ClaimOutcome;
use std::sync::{Arc, Mutex};

/// One agent joined to the channel under the member ID and nick Velocity
/// gave it. Cheap to clone: clones share the session and the read cursor.
#[derive(Clone)]
pub struct ChannelMembership {
    session: Session,
    member: String,
    machine: String,
    /// How far [`ChannelMembership::unread`] has read: a delivery count, not
    /// a message number, so a held message released later is still read.
    read_cursor: Arc<Mutex<u64>>,
}

/// What `airc_claim` got: the piece, and the files as path claims (R6) on
/// the agent's change. Files are claimed only once the piece is granted, so
/// a refused piece never leaves paths claimed behind it; and refused files
/// release a piece this call newly claimed, so they never leave the piece
/// held behind them.
#[derive(Debug, Clone)]
pub struct WorkClaim {
    /// `Granted` with `extended: false` and refused `files` means the piece
    /// was granted and released again: the member does not hold it.
    pub piece: PieceOutcome,
    /// `None` when the piece was refused or no files were named.
    pub files: Option<ClaimOutcome>,
}

impl WorkClaim {
    /// Whether the piece and every named file were granted.
    pub fn granted(&self) -> bool {
        matches!(self.piece, PieceOutcome::Granted { .. })
            && !matches!(self.files, Some(ClaimOutcome::Refused { .. }))
    }
}

impl Session {
    /// Joins the channel as `member` from `machine`, under this session's
    /// agent as nick, or rejoins it. A rejoin keeps the member's message
    /// mode, so a quarantine survives a restart.
    pub async fn join_channel(
        &self,
        member: impl Into<String>,
        machine: impl Into<String>,
    ) -> Result<ChannelMembership, Error> {
        let membership = ChannelMembership {
            session: self.clone(),
            member: member.into(),
            machine: machine.into(),
            read_cursor: Arc::new(Mutex::new(0)),
        };
        membership.rejoin().await?;
        Ok(membership)
    }
}

/// Presence and message mode.
impl ChannelMembership {
    pub fn member(&self) -> &str {
        &self.member
    }

    /// The nick other agents address this member by: the session's agent.
    pub fn nick(&self) -> &str {
        self.session.agent().as_str()
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Joins again after [`ChannelMembership::leave`] or a server restart.
    pub async fn rejoin(&self) -> Result<Member, Error> {
        self.session
            .client()
            .channel_join(&self.member, self.nick(), &self.machine)
            .await
    }

    /// Marks the member gone. Its piece claims stay until released.
    pub async fn leave(&self) -> Result<Member, Error> {
        self.session.client().channel_leave(&self.member).await
    }

    pub async fn set_mode(&self, mode: MessageMode) -> Result<Member, Error> {
        self.session
            .client()
            .channel_set_mode(&self.member, mode)
            .await
    }

    /// Every member that ever joined, joined ones first.
    pub async fn presence(&self) -> Result<Vec<Member>, Error> {
        self.session.client().channel_presence().await
    }
}

/// Piece claims, with files as path claims.
impl ChannelMembership {
    /// First claim wins (R13). `piece` is the work item's identity, such as
    /// `task:<id>`; without one the task text is the lock. Claiming a held
    /// piece again extends it and keeps its original claim time.
    pub async fn claim_piece(
        &self,
        piece: Option<&str>,
        task: &str,
        branch: &str,
    ) -> Result<PieceOutcome, Error> {
        self.session
            .client()
            .claim_piece(&self.member, piece, task, branch)
            .await
    }

    /// `airc_claim` with files: claims the piece, then claims `files` for
    /// `change`, every file or none. The files' claim lives on the change,
    /// so it releases when the change lands or is abandoned (R6).
    pub async fn claim_work(
        &self,
        piece: Option<&str>,
        task: &str,
        branch: &str,
        change: &str,
        files: &[String],
    ) -> Result<WorkClaim, Error> {
        let piece = self.claim_piece(piece, task, branch).await?;
        let new_claim = match &piece {
            PieceOutcome::Granted { claim, extended } if !files.is_empty() => {
                (!extended).then(|| claim.piece.clone())
            }
            _ => return Ok(WorkClaim { piece, files: None }),
        };
        let files = self.session.claim(change, files).await;
        // A caller reads a refusal as "pick other work" (AGENTS.md), so a
        // piece claimed only for these files must not stay locked behind
        // them (issue #188). A piece the member already held stays held.
        let granted = matches!(files, Ok(ClaimOutcome::Granted { .. }));
        if let Some(new_piece) = new_claim.filter(|_| !granted) {
            let released = self.release_piece(Some(&new_piece)).await;
            // The claim's own error, returned below, explains more than a
            // release failing after it.
            if files.is_ok() {
                released?;
            }
        }
        Ok(WorkClaim {
            piece,
            files: Some(files?),
        })
    }

    /// Releases one of this member's pieces, or all of them with `None`.
    /// Returns the claims it dropped.
    pub async fn release_piece(&self, piece: Option<&str>) -> Result<Vec<PieceClaim>, Error> {
        self.session
            .client()
            .release_piece(&self.member, piece)
            .await
    }

    /// Every held piece, oldest claim first.
    pub async fn pieces(&self) -> Result<Vec<PieceClaim>, Error> {
        self.session.client().pieces().await
    }
}

/// Messages and ledgers.
impl ChannelMembership {
    /// Sends `text` to the nicks in `to`. A message naming an unknown nick
    /// is refused whole.
    pub async fn send(
        &self,
        to: &[&str],
        intent: Intent,
        text: impl Into<String>,
    ) -> Result<ChannelMessage, Error> {
        let message = SendMessage {
            from: self.author(),
            to: to.iter().map(|nick| (*nick).to_owned()).collect(),
            intent,
            text: text.into(),
        };
        self.session.client().channel_send(message).await
    }

    /// Messages delivered to this member after `after`, a cursor from an
    /// earlier page. Velocity uses this when it keeps the cursor itself.
    pub async fn messages_after(&self, after: u64) -> Result<MessagePage, Error> {
        self.session
            .client()
            .channel_messages(Some(&self.member), after)
            .await
    }

    /// Messages delivered since the last call on this handle or its clones.
    /// The cursor moves only after the page arrives, so a failed read loses
    /// nothing.
    pub async fn unread(&self) -> Result<Vec<ChannelMessage>, Error> {
        let after = *self.lock_cursor();
        let page = self.messages_after(after).await?;
        let mut cursor = self.lock_cursor();
        // A concurrent read on a clone may have gone further already.
        *cursor = (*cursor).max(page.cursor);
        Ok(page.messages)
    }

    /// Delivers the messages held while this member was heads-down.
    pub async fn release_held(&self) -> Result<u64, Error> {
        self.session
            .client()
            .channel_release_held(&self.member)
            .await
    }

    /// Appends to a ledger. `velocity_issue` titles a discovery about
    /// Velocity itself, which Velocity files on its own repository.
    pub async fn append_ledger(
        &self,
        ledger: Ledger,
        text: impl Into<String>,
        velocity_issue: Option<String>,
    ) -> Result<LedgerEntry, Error> {
        let entry = AppendEntry {
            ledger,
            author: self.author(),
            text: text.into(),
            velocity_issue,
        };
        self.session.client().ledger_append(entry).await
    }

    /// Ledger entries after `after`, oldest first.
    pub async fn read_ledger(&self, ledger: Ledger, after: u64) -> Result<Vec<LedgerEntry>, Error> {
        self.session.client().ledger_read(ledger, after).await
    }

    fn author(&self) -> Author {
        Author::Agent {
            member: self.member.clone(),
        }
    }

    fn lock_cursor(&self) -> std::sync::MutexGuard<'_, u64> {
        // The cursor is a plain number, so a panic elsewhere cannot leave it
        // half-written; recover it rather than poison every later read.
        self.read_cursor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
