### R13 · AIRC channel on the server
The server holds each project's AIRC channel, so agents on every machine coordinate through one source of truth.
AIRC's behaviour (claims, events, targeted delivery, messages, message modes and ledgers) is as Velocity SPEC F14 defines it.
Rosen is its store and its authority.
- **State.**
  The channel's state lives in the server's metadata store:
  - **piece claims** on work items (`task:<id>`, `issue:<n>`, `spec:<section>`, or the task text);
  - **file claims, shares and waits**, which are Rosen path claims (R6);
  - **events** and agent **messages** with their `to` and `intent`;
  - **presence**: the nicks joined, their machine, and each one's message mode;
  - the **Discoveries** and **Past work** ledgers.
- **Piece claims** are first-claim-wins, each in one transaction, like exclusive path claims.
  A refusal names the holder and the time of its claim.
  A claim is kept through failure and disconnects, and released as Velocity F14 says.
  - A piece is locked by its identity, or by its task text when it has none.
    The holder claiming again extends its claim and keeps the original claim time.
  - A release drops only the caller's own pieces: one, or all of them.
- **Presence.**
  A member joins with the member ID, nick and machine Velocity gave it.
  A nick names one joined member at a time.
  Leaving keeps the member's piece claims.
  Rejoining keeps its message mode, so a restart does not lift a quarantine.
- **Messages.**
  Recipients are resolved and fixed when a message is sent.
  - A message naming an unknown nick is refused whole.
    Only the user's comments may name no one, and they then reach every joined member.
  - A heads-down member's messages from other agents are held until Velocity releases them or the member returns to open mode.
    They are delivered after its read cursor, so none is skipped.
  - A quarantined member gets nothing from other agents and its messages reach nobody.
    Its piece claims are frozen: it cannot take, extend or release one.
- **One log.**
  Channel events and Rosen's events share one ordered log and one cursor per subscriber (R9).
  Targeted delivery follows Velocity F14: an agent receives only the lines it must act on.
  - Channel lines use the `channel` event kind with the channel payload in `detail`.
    Their recipient agent identities are fixed at publication; an empty audience reaches only the project view.
    They use the same subscription and acknowledgement API as Rosen events, including over the network transport.
  - Each channel write commits in one transaction with its lines, so a stored change never lacks its line.
    A delivered message's line targets its recipients by nick.
    Joins, leaves, mode changes, claims, refusals, releases and ledger entries reach only the project view, so an agent is pushed only what it must act on.
- **Velocity keeps the tools.**
  Velocity serves AIRC's tools to every agent as now, with the member ID and nick fixed at launch, and calls the server through `rosen-client` for each one.
  No machine keeps channel state of its own beyond its delivery cursors.
- **Server unreachable.**
  Claims and messages need the server, as landing does.
  A tool call made while the server is unreachable fails with that reason.
  It is never answered from a local copy.
- **Ledger push.**
  When the base's `rosen.toml` names a root `ledger_endpoint`, the server sends new Discoveries and Past work entries there, such as Collapse's `https://api.collapse.sh/<owner>/<name>/ledgers`.
  Without `ledger_endpoint`, a root `collapse` URL (R9 state feed) sends them to `<collapse>/ledgers`, so one setting serves both feeds.
  The URL is `https` with no credentials in it; plain `http` is accepted only for a loopback host.
  The token comes from git's credential helper on the server, the one the mirror push uses (R1), and goes as HTTPS basic auth.
  - One background task sends, off the append path, as the mirror push runs after a land.
    An append never waits for it and never fails because of it.
  - Each request is a JSON batch `{"entries":[...]}` of at most 1,000 entries and 2 MB, in sequence order.
    An entry carries its ledger, sequence, author nick, machine, UTC time and text, plus the Rosen or Velocity issue title a discovery has.
    Text, names and titles longer than the endpoint accepts are cut to fit.
  - Only the two ledgers are read, so claims, events, messages and presence are never sent.
    Velocity's versioned records inside the ledgers (`velocity-airc/` envelopes and older bare JSON checkouts, task results and delivery acknowledgements) are skipped; a discovery in Velocity's envelope is sent as its text and issue title.
  - The endpoint stores entries by sequence and answers with the highest sequence it holds in each ledger.
    The server stores how far each ledger has reached each endpoint, so a restart resumes there.
    On start and after a failure it first sends an empty batch to read what the endpoint holds, and resends from there when that is less than it delivered.
  - A failed send is retried with backoff from 1 s to 5 minutes and publishes a `ledger_push` event (R9); the first success after failures publishes one too.
    When the endpoint refuses a batch with HTTP 400, 413 or 422, the push splits the batch by halves to isolate the refused entry(s), skips them, publishes `ledger_push` `status: "refused"` naming the entry, and keeps delivering the rest; health counts `last_success_at` only for delivered batches.
    HTTP 410 (the endpoint's repository is archived) stops sending until the endpoint changes or the server restarts.
  - Without `ledger_endpoint` or `collapse`, nothing is sent.
  - Only the mirror-owner server (R1) pushes ledger entries; any other server sends nothing.
- **Accept when:** two agents on different machines that claim the same piece or an exclusive path at the same moment never both succeed; an event or message reaches its recipient on another machine within 1 s while both machines reach the server; and no two machines ever see different claim holders.
