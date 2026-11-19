### R9 · Events
- File-backed event-stream reads use pooled read-only WAL snapshots, so subscriber page scans do not hold the event writer needed to announce a land.
  Each page is read consistently; publication, sequence ordering and cursor writes still use the single writer.
- Staged-event drains take turns separately from the event writer, avoiding duplicate audience computation while other publishers and readers continue.
  Each caller still drains after its transaction commits, including after an earlier drain failed.
- Audience selection reuses bounded, in-memory tree and line diffs keyed by exact parent and head objects.
  Literal path comparisons avoid glob construction.
  Claims, live states and overlap policy are read again for each selection; object reads never hold the cache lock.
- A `rebased` event reports an installed rebase, including a stack restack or an owner-requested rebase.
  A clean overlap probe emits none.
  Claim-only overlaps receive the `landed` notice.
- The event types are `snapshot`, `landed`, `rebased`, `conflict`, `updated`, `review`, `claim`, `released`, `green`, `built`, `reverted`, `revert_refused`, `post_land_failure`, `mirror` and `ledger_push` (R13).
- Each event is delivered only to subscribers it concerns: the change's owner, its reviewers, changes stacked on it, and the holders of overlapping changes or claims.
- An event's audience is fixed when it is published.
  An event about no change, such as `mirror` or `ledger_push`, goes to every subscriber.
- A subscriber names its agent.
  A subscription that names no agent is the project view: it gets every event and stores no cursor.
- Delivery is at least once and resumes from a cursor after reconnect.
  Cursors and the event log are stored on the server, so they survive a server restart.
  An event commits with the change it reports, so a server that stops before storing it delivers it at its next start.
  - A subscriber acknowledges the events it has handled, and its stored cursor moves only then, never backwards.
    A subscription with no explicit cursor resumes after the stored one.
- Rosen's events and the channel's events (R13) share one log, one sequence and one cursor per subscriber.
  A claim release and the land that caused it are always delivered in order, to subscribers on any machine.
