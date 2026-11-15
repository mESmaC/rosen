### R7 · Review
- Review comments anchor to file, line and change head, and form threads with an open or resolved state.
  - A new thread anchors to a line of a file in the change's current head, and the server refuses a path that is not a file there or a line the file lacks.
    A comment that names the head it was written against is refused once the change has a different head.
    The anchor never moves; each reply records the head it was written at.
  - Open and ready changes take comments; after-land projects also accept them on landed changes.
    Anyone can resolve a thread.
    A reply to a resolved thread reopens it.
    Each comment, resolve and reopen publishes a `review` event.
- `review approve <change> --head <head> [--reviewer <agent>]` refuses stale heads and abandoned changes (also landed changes in `before-land` mode), stores the reviewer and timestamp, and publishes a `review` event.
  `review list <change> --approvals` lists durable approval history, including void approvals.
- An approval is tied to the head it approved.
  A new head by the author voids the approval.
  A rebase done by the server (R8, D19) keeps it, because it changes only the parent.
  A receipt likewise survives when its checked tree still equals the current merge result (R8).
- **Review tiers** follow Anthropic's semantic approval rules and Velocity F7:
  - The server tiers each change from its diff: prose-only, normal, or sensitive.
  - Sensitive paths are listed in project configuration and read from the base, so a change cannot lower its own tier.
    Sensitive matches take precedence, including removed paths.
  - Prose-only means all changed files are regular UTF-8 `.md`, `.markdown`, `.txt` or `.rst` files without NUL bytes on either side.
    Generated files, registration lists and configured workspace inputs take normal review.
    Other edits, including source comments and executable or symlink modes, conservatively take normal review.
    The diff is against the parent (the stack parent for a stacked change); an empty diff is prose-only.
  - In `before-land` mode every tier still needs an approval.
    In `after-land` mode no tier needs approval or a receipt.
- Velocity's PR duty reviews through `rosen-client`.
- `review mark <change> <unreviewed|reviewed|issues-filed>` records coverage on a landed change; `review landed [--state <state>]` lists it.
  Comments, threads, resolutions and approvals work on open and, in after-land projects, landed changes.
- **Accept when:** in `before-land` mode a change cannot land without an approval for its current head; in `after-land` mode an unapproved change lands and its review coverage remains visible.
