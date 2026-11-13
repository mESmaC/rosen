//! Risk: a wrong piece-claim rule lets two agents both own a piece of work,
//! or lets a release drop another agent's claim (SPEC R13). Every history in
//! the table exported from `proofs/PieceClaims/Export.lean` is replayed
//! against the server's channel store, operation by operation.

use rosen_protocol::channel::{ChannelAnswer, ChannelRequest, PieceOutcome};
use rosen_server::channel::ChannelService;

fn run(channel: &ChannelService, op: &str) -> String {
    let (kind, rest) = op.split_at(1);
    let (member, piece) = rest.split_at(1);
    let member = member.to_owned();
    let call = match kind {
        "c" => ChannelRequest::ClaimPiece {
            member,
            piece: Some(piece.to_owned()),
            task: "fixture".into(),
            branch: "fixture".into(),
        },
        "r" => ChannelRequest::ReleasePiece {
            member,
            piece: (piece != "*").then(|| piece.to_owned()),
        },
        _ => panic!("bad op {op:?}"),
    };
    match channel.run(call).expect("channel call") {
        ChannelAnswer::Piece(PieceOutcome::Granted { extended, .. }) => {
            if extended { "e" } else { "g" }.to_owned()
        }
        ChannelAnswer::Piece(PieceOutcome::Refused { holder }) => format!("r{}", holder.member),
        ChannelAnswer::Pieces(released) => released.len().to_string(),
        other => panic!("unexpected answer {other:?}"),
    }
}

fn holders(channel: &ChannelService) -> String {
    let ChannelAnswer::Pieces(held) = channel.run(ChannelRequest::Pieces).unwrap() else {
        panic!("pieces answer");
    };
    ["a", "b"]
        .map(|piece| {
            let holder = held.iter().find(|claim| claim.piece == piece);
            format!(
                "{piece}={}",
                holder.map_or("-", |claim| claim.member.as_str())
            )
        })
        .join(" ")
}

#[test]
fn piece_claims_match_the_lean_fixture_table() {
    let table = include_str!("../../../proofs/fixtures/piece-claims.tsv");
    let mut rows = 0;
    for line in table.lines().skip(1) {
        let [history, outcomes, expected] = line.split('\t').collect::<Vec<_>>()[..] else {
            panic!("bad row {line:?}");
        };
        let channel = ChannelService::in_memory();
        for member in ["1", "2"] {
            let join = ChannelRequest::Join {
                member: member.into(),
                nick: format!("n{member}"),
                machine: "fixture".into(),
            };
            channel.run(join).unwrap();
        }
        let got: Vec<String> = history.split(' ').map(|op| run(&channel, op)).collect();
        assert_eq!(got.join(" "), outcomes, "outcomes: {line}");
        assert_eq!(holders(&channel), expected, "holders: {line}");
        rows += 1;
    }
    assert!(rows > 0, "the fixture table is empty");
}
