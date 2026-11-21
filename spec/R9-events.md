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
- The event types are `snapshot`, `landed`, `rebased`, `conflict`, `updated`, `review`, `claim`, `released`, `green`, `built`, `deploy_due`, `deploy_result`, `reverted`, `revert_refused`, `post_land_failure`, `bisect`, `mirror`, `ledger_push` (R13) and `state_push`.
- Each event is delivered only to subscribers it concerns: the change's owner, its reviewers, changes stacked on it, and the holders of overlapping changes or claims.
- An event's audience is fixed when it is published.
  An event about no change, such as `mirror`, `ledger_push` or `state_push`, goes to every subscriber.
  A `bisect` event concerns no agent and goes only to the project view.
- A subscriber names its agent.
  A subscription that names no agent is the project view: it gets every event and stores no cursor.
- Delivery is at least once and resumes from a cursor after reconnect.
  Cursors and the event log are stored on the server, so they survive a server restart.
  An event commits with the change it reports, so a server that stops before storing it delivers it at its next start.
  - A subscriber acknowledges the events it has handled, and its stored cursor moves only then, never backwards.
    A subscription with no explicit cursor resumes after the stored one.
- Rosen's events and the channel's events (R13) share one log, one sequence and one cursor per subscriber.
  A claim release and the land that caused it are always delivered in order, to subscribers on any machine.
- **State feed.**
  When the base's `rosen.toml` names a root `collapse` URL, such as `https://api.collapse.sh/<owner>/<name>`, the server sends its live state to `<collapse>/rosen/state`.
  The URL follows the ledger push's rules (R13): `https` with no credentials, and the token from git's credential helper as HTTPS basic auth.
  - One background task sends, with the ledger push's sender and backoff, so a land, snapshot or review call never waits for it and never fails because of it.
  - A cursor over this event log, stored in the metadata store, says what is dirty.
    An event naming a change makes that change dirty, and a `claim` also the changes it overlaps.
    `landed`, `review`, `green` and `reverted` make review coverage dirty, and `landed` the configuration in force.
    `conflict`, `built`, `green`, `reverted`, `revert_refused`, `post_land_failure`, `bisect` and `mirror` events are sent as event lines.
    A channel `joined`, `left` or `mode` line makes presence dirty.
    Other channel lines are sent only with `collapse_channel = true`, and push reports make nothing dirty.
  - Nothing is sent on a timer, so an idle project wakes no endpoint.
    Sends start at most once every 10 s, about 2 s after the first dirtying event; one holding only snapshots waits up to 60 s.
    The machine list and presence ride on the next send, or else go 60 s after their last change.
  - A request is a JSON batch `{"format":1,"feed_id":"...","through":N,"changes":[...],"conflicts":[...],"threads":[...],"coverage":[...],"config":[...],"events":[...],"machines":[...],"presence":[...],"channel":[...]}` of at most 2 MB.
    `feed_id` is a persistent random identifier per server data directory sent with every state and ledger batch.
    Each item is the whole current document, not a delta, and carries `seq`, the event sequence it reflects.
    A change's conflicts and threads are items of their own, so a large change splits across batches; an item too large for any batch is sent as a `too_large` marker.
    `through` is the sequence the endpoint holds everything through once it stores the batch, and only a pass's last batch raises it.
  - A batch also carries `"relay":{"allow":[...]}`, the action kinds the server runs for Collapse (R14), on start and whenever the setting or the `collapse` machine's status changed.
    An answer holding `"actions_pending":true` makes the relay fetch actions at once.
  - When a check set names required platforms (R12), a batch also carries `"platforms":{...}` with the base checkpoints, each required platform's built and green checkpoints, `checker_connected` and the connected checkers, resent whenever a checkpoint changes or a checker connects or disconnects.
  - A change document holds its ID, `owner@machine`, state, parent, head, title (the newest named snapshot's message), creation and update times, the land check (R8) with its verdicts, tier and reach, conflict and thread IDs, approvals including void ones, the latest receipt without its signature and `base_current`, edit regions per path with lines added and removed against the parent (R6, R11), its path claims as `notice`, `exclusive`, `share` or `wait`, the changes overlapping it with their paths, and its newest 20 named snapshots with the total snapshot count.
    A part that cannot be read is named in `unavailable`.
  - A conflict item holds its path, start line, open or resolved state, and base, ours and theirs each cut to 64 KiB with a `truncated` flag, with the base merged in and the time the `conflict` event that stored it was published.
    A `conflict` event names that base in `detail.onto` when a land or a rebase stored it; otherwise it is the base the preceding `rebased` event named.
  - A landed or abandoned change is sent once, with its landed commit, and then only again if it reopens.
  - Coverage sends each landed change's reviewed state (R7) when it changed, and a removal when a revert takes a change off the list.
    The configuration item holds the base commit, source, error and parsed configuration, and is sent again only when what is in force changes.
  - An event line holds the event's own `seq`, kind, change, detail and `time`, the Unix milliseconds the log stored it.
    It is sent once, in sequence order, from the same cursor.
    A `conflict` line's detail keeps only its paths and `onto`, a `mirror` line's leaves out the remote, and the `output` of a `post_land_failure` or `reverted` line keeps its last 64 KiB with a `truncated` flag.
  - The machines item lists every machine (R14) with its ID, name, role, status, running agents, credential expiry, request time, `last_contact` rounded down to the minute and `out_of_contact` once an active machine has gone 5 minutes without contact.
    Machine changes publish no event, so while `collapse` is set the server compares the list with what it last saw every 10 s, a local read only.
    A change in anything but `last_contact` makes it dirty.
  - The presence item lists the joined members (R13) by nick, machine, mode and `joined_at`, with `as_of`.
    While any member is joined, it also rides on an outgoing send at most every 5 minutes, though nothing is sent just to refresh it.
  - With `collapse_channel = true` in the base's `rosen.toml`, every `channel` line but a ledger entry is sent once, as R13 stores it, with its `seq` and `time`.
    It is off by default, since channel lines hold agent messages; ledger entries travel only through the ledger push (R13).
  - The server keeps a fingerprint of each document it delivered and skips a dirty document the endpoint already holds.
  - The endpoint keeps the newer of two copies by `seq`, so a repeated batch changes nothing, and answers `{"highest_sequence":N}`.
    On start and after a failure the server first sends an empty batch to read that, resumes from it when it is less than was delivered, and then resends every live document.
  - A failed send is retried with backoff from 1 s to 5 minutes and publishes a `state_push` event with `status: "failed"`; the first success after failures publishes one with `status: "sent"`, `sent`, `delivered` and `after_failures`, matching `ledger_push` (R13).
    When the endpoint refuses a batch with HTTP 400, 413 or 422, the feed splits the batch by halves to isolate the refused item(s), skips each refused document until its content changes, publishes `state_push` `status: "refused"` naming the document, and keeps delivering the rest.
    Health records `last_success_at` only for delivered batches, not empty reconciliation probes.
    HTTP 409 `source_changed` (or Collapse reporting `highest_sequence` ahead of this server's local event log) publishes `status: "diverged"`, reports `diverged` in `server status` and `collapse check`, and sends nothing until the operator runs `rosen collapse link --reset`.
    HTTP 410 publishes `status: "stopped"` and stops sending until `collapse` changes or the server restarts.
  - Receipt signatures, machine credentials and their hashes, join keys, network addresses, certificate fingerprints and the data directory are never sent.
    Messages and piece claims are sent only as channel lines with `collapse_channel = true`.
  - Without `collapse`, nothing is sent.
  - Only the mirror-owner server (R1) sends the state feed; any other server sends nothing to Collapse.
  - Golden wire fixtures for wire schema `v1` (`"format": 1`) live in `fixtures/collapse/v1/`, covering each state-feed unit kind, the ledger push (R13) and the action relay (R14).
    A drift test builds every shape with the real sender code and compares parsed JSON against those files, printing the full actual JSON on a mismatch and rewriting them when run with `UPDATE_FIXTURES=1`.
