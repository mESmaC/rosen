### R9 · Events
- A `rebased` event reports an installed rebase, including a stack restack or an owner-requested rebase.
  A clean overlap probe emits none.
  Claim-only overlaps receive the `landed` notice.
- The event types are `snapshot`, `landed`, `rebased`, `conflict`, `updated`, `review`, `claim`, `released`, `green`, `built`, `reverted`, `post_land_failure` and `mirror`.
- Each event is delivered only to subscribers it concerns: the change's owner, its reviewers, changes stacked on it, and the holders of overlapping changes or claims.
- An event's audience is fixed when it is published.
  An event about no change, such as `mirror`, goes to every subscriber.
- A subscriber names its agent.
  A subscription that names no agent is the project view: it gets every event and stores no cursor.
- Delivery is at least once and resumes from a cursor after reconnect.
  Cursors and the event log are stored on the server, so they survive a server restart.
  An event commits with the change it reports, so a server that stops before storing it delivers it at its next start.
  - A subscriber acknowledges the events it has handled, and its stored cursor moves only then, never backwards.
    A subscription with no explicit cursor resumes after the stored one.
- Rosen's events and the channel's events (R13) share one log, one sequence and one cursor per subscriber.
  A claim release and the land that caused it are always delivered in order, to subscribers on any machine.
