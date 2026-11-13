//! Typed calls for the project's AIRC channel (SPEC R13). Velocity calls one
//! of these for each AIRC tool. Each goes to the server, through this
//! machine's daemon. While the server is unreachable the call fails with
//! that reason, and it is never answered from a local copy.

use crate::connection::{unexpected, Client};
use crate::Error;
use rosen_protocol::channel::{
    AppendEntry, ChannelAnswer, ChannelMessage, ChannelRequest, Ledger, LedgerEntry, Member,
    MessageMode, MessagePage, PieceClaim, PieceOutcome, SendMessage,
};
use rosen_protocol::{Request, Response};

impl Client {
    /// Runs one channel operation and returns its answer.
    pub async fn channel(&self, call: ChannelRequest) -> Result<ChannelAnswer, Error> {
        match self.request(Request::Channel(call)).await? {
            Response::Channel(answer) => Ok(answer),
            other => Err(unexpected(other)),
        }
    }

    pub async fn channel_join(
        &self,
        member: &str,
        nick: &str,
        machine: &str,
    ) -> Result<Member, Error> {
        let call = ChannelRequest::Join {
            member: member.to_owned(),
            nick: nick.to_owned(),
            machine: machine.to_owned(),
        };
        member_of(self.channel(call).await?)
    }

    pub async fn channel_leave(&self, member: &str) -> Result<Member, Error> {
        let call = ChannelRequest::Leave {
            member: member.to_owned(),
        };
        member_of(self.channel(call).await?)
    }

    pub async fn channel_set_mode(&self, member: &str, mode: MessageMode) -> Result<Member, Error> {
        let call = ChannelRequest::SetMode {
            member: member.to_owned(),
            mode,
        };
        member_of(self.channel(call).await?)
    }

    pub async fn channel_presence(&self) -> Result<Vec<Member>, Error> {
        match self.channel(ChannelRequest::Presence).await? {
            ChannelAnswer::Members(members) => Ok(members),
            other => Err(unexpected_answer(other)),
        }
    }

    /// First claim wins. A refusal is [`PieceOutcome::Refused`] naming the
    /// holder and its claim time, not an error.
    pub async fn claim_piece(
        &self,
        member: &str,
        piece: Option<&str>,
        task: &str,
        branch: &str,
    ) -> Result<PieceOutcome, Error> {
        let call = ChannelRequest::ClaimPiece {
            member: member.to_owned(),
            piece: piece.map(str::to_owned),
            task: task.to_owned(),
            branch: branch.to_owned(),
        };
        match self.channel(call).await? {
            ChannelAnswer::Piece(outcome) => Ok(outcome),
            other => Err(unexpected_answer(other)),
        }
    }

    /// Releases one of the member's pieces, or all of them with `None`.
    pub async fn release_piece(
        &self,
        member: &str,
        piece: Option<&str>,
    ) -> Result<Vec<PieceClaim>, Error> {
        let call = ChannelRequest::ReleasePiece {
            member: member.to_owned(),
            piece: piece.map(str::to_owned),
        };
        pieces_of(self.channel(call).await?)
    }

    pub async fn pieces(&self) -> Result<Vec<PieceClaim>, Error> {
        pieces_of(self.channel(ChannelRequest::Pieces).await?)
    }

    pub async fn channel_send(&self, message: SendMessage) -> Result<ChannelMessage, Error> {
        match self.channel(ChannelRequest::Send(message)).await? {
            ChannelAnswer::Sent(message) => Ok(message),
            other => Err(unexpected_answer(other)),
        }
    }

    /// Messages delivered to `reader` after `after`, or every message.
    pub async fn channel_messages(
        &self,
        reader: Option<&str>,
        after: u64,
    ) -> Result<MessagePage, Error> {
        let call = ChannelRequest::Messages {
            reader: reader.map(str::to_owned),
            after,
        };
        match self.channel(call).await? {
            ChannelAnswer::Messages(page) => Ok(page),
            other => Err(unexpected_answer(other)),
        }
    }

    /// Delivers the messages held for a heads-down member.
    pub async fn channel_release_held(&self, member: &str) -> Result<u64, Error> {
        let call = ChannelRequest::ReleaseHeld {
            member: member.to_owned(),
        };
        match self.channel(call).await? {
            ChannelAnswer::Delivered(count) => Ok(count),
            other => Err(unexpected_answer(other)),
        }
    }

    pub async fn ledger_append(&self, entry: AppendEntry) -> Result<LedgerEntry, Error> {
        match self.channel(ChannelRequest::Append(entry)).await? {
            ChannelAnswer::Entry(entry) => Ok(entry),
            other => Err(unexpected_answer(other)),
        }
    }

    pub async fn ledger_read(&self, ledger: Ledger, after: u64) -> Result<Vec<LedgerEntry>, Error> {
        match self
            .channel(ChannelRequest::ReadLedger { ledger, after })
            .await?
        {
            ChannelAnswer::Entries(entries) => Ok(entries),
            other => Err(unexpected_answer(other)),
        }
    }
}

fn member_of(answer: ChannelAnswer) -> Result<Member, Error> {
    match answer {
        ChannelAnswer::Member(member) => Ok(member),
        other => Err(unexpected_answer(other)),
    }
}

fn pieces_of(answer: ChannelAnswer) -> Result<Vec<PieceClaim>, Error> {
    match answer {
        ChannelAnswer::Pieces(pieces) => Ok(pieces),
        other => Err(unexpected_answer(other)),
    }
}

fn unexpected_answer(answer: ChannelAnswer) -> Error {
    unexpected(Response::Channel(answer))
}
