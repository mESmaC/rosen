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
- **Accept when:** two agents on different machines that claim the same piece or an exclusive path at the same moment never both succeed; an event or message reaches its recipient on another machine within 1 s while both machines reach the server; and no two machines ever see different claim holders.
