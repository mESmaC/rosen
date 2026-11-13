//! Piece claims on work items (SPEC R13): the AIRC channel's task lock.
//!
//! A piece is named by its identity (`task:<id>`, `issue:<n>`,
//! `spec:<section>`) or, without one, by its task text. The first member to
//! claim a piece holds it until it is released. The server runs [`decide`]
//! inside one transaction per claim, so two claims on one piece are
//! serialised and never both succeed. `proofs/PieceClaims/Design.lean`
//! models this rule.

/// The lock key of a piece: its identity when it has one, otherwise its task
/// text. Surrounding whitespace is dropped, so the same identity typed with
/// a stray space still collides instead of creating a second lock. `None`
/// when both are blank: such a claim would lock nothing.
pub fn piece_key(piece: Option<&str>, task: &str) -> Option<String> {
    let key = match piece.map(str::trim) {
        Some(identity) if !identity.is_empty() => identity,
        _ => task.trim(),
    };
    (!key.is_empty()).then(|| key.to_owned())
}

/// What a claim on one piece does, given who holds it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceDecision {
    /// Nobody holds it: the claimant takes it.
    Grant,
    /// The claimant already holds it: the claim is extended in place and
    /// keeps its original time, so first-claim order never changes.
    Extend,
    /// Another member holds it: the claim is refused, naming that holder.
    Refuse,
}

/// First claim wins.
pub fn decide(holder: Option<&str>, claimant: &str) -> PieceDecision {
    match holder {
        None => PieceDecision::Grant,
        Some(member) if member == claimant => PieceDecision::Extend,
        Some(_) => PieceDecision::Refuse,
    }
}
