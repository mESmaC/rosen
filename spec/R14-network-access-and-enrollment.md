### R14 · Network access and enrollment
Machines join a project with no login, monitor or remote desktop on the joining machine, so headless machines can be added from a script.
- **Network listener.**
  The server listens on a TCP port with TLS, alongside the local transport.
  Its certificate is created when the project is created.
  Daemons on other machines connect to it, and serve their own machine's clients over the local transport.
- **Join keys.**
  The project owner creates a join key from any enrolled machine, through Velocity or `rosen machine key create`.
  A join key has an expiry, a use limit and a role (agent machine or checker), and carries the server's address and the fingerprint of its certificate.
- **Zero-touch join.**
  A machine joins with one non-interactive command, such as `rosen machine join <key>` in an install or provisioning script, or a machine image.
  1. The daemon connects to the server and checks the server's certificate against the key's fingerprint.
     On a mismatch it stops before sending the key.
  2. It exchanges the key for the machine's own **machine credential**, sending its receipt public key, which the server binds to the machine as it issues the credential (R8 step 1).
  3. It stores the credential encrypted (Windows: DPAPI through Credential Manager).
     The client rotates it automatically before it expires.

  The machine never uses the join key again.
  Credentials last 30 days; the daemon checks hourly and renews during the final seven days.
  A lost rotation response can be retried with the prior credential until its original expiry.
  The server stores only credential hashes.
  Windows Credential Manager holds the encrypted enrollment; `upstream.json` holds only a reference, address and certificate fingerprint.
  The server's `network/identity.json` (its TLS private key) and `upstream.json` are created readable and writable by the current OS user only (Windows: a protected access list naming that user and SYSTEM; Unix: mode 0600), whatever the data directory allows.
  Enrollment and pending requests alone are accepted without an authenticated machine; every other network request rechecks the machine's current status.
- **Existing enrollment files.**
  Before reading an existing `network/identity.json` or `upstream.json`, the daemon reapplies the same owner-only permissions, including to files created by older versions.
  If it cannot protect the file, loading fails before the identity or upstream settings are used.
  Repair preserves the file contents and does not automatically rotate the identity, keeping enrolled certificate pins valid.
  A previously accessible key must be treated as possibly disclosed: permission repair cannot undo prior access; recovery from suspected compromise requires replacing the identity and re-enrolling machines.
- **Approval.**
  A machine that joins with a valid key is admitted at once.
  A machine that asks to join without one waits as **pending** until the owner approves it from an enrolled machine.
  - `rosen machine join --address <host:port> --fingerprint <SHA-256>` requests pending admission without a key.
    `--name` names the machine and `--data-dir` selects its local daemon data directory.
    The pending credential is stored encrypted; approval activates it without another join.
    A revoked machine must enroll again and cannot be reactivated by approval.
    Because anyone who reaches the listener can ask, at most 64 requests await approval at once, at most 4 from one address (an IPv6 /64 counts as one); a request past either limit is refused until the owner approves or revokes some.
    An unapproved request expires, and is removed, 7 days after it was made.
- **Machine list and revocation.**
  The server lists every machine with its role, last contact (to the minute) and running agents.
  It also gives an active machine's credential expiry and a pending machine's request time.
  `rosen machine list` shows "rotates in N d" for an active machine, "renewal overdue, expires in N d" once its credential is inside the seven-day renewal window, and "pending since <time>, expires in N d" for a pending request.
  Revoking a machine refuses its credential on its next request.
  Its changes, claims and workspaces are kept, as for a failed change.
- **Discovery.**
  On a local network, daemons find the server by DNS-SD (mDNS), so a key without a reachable address still works there.
  - The listener advertises `_rosen._tcp.local.` with its certificate fingerprint.
    Joining tries the key's address first, then matching LAN announcements; every candidate must pass TLS certificate pinning before receiving the key.
    `machine key create --address` can supply an address reachable from the joining machine.
- **Agents.**
  Every agent on a machine works under that machine's credential.
  Velocity names each agent (its nick) and its role when it starts the agent's session, as AIRC does now.
  Owners, reviewers and receipt submitters are recorded as machine plus nick.
  The request names only the nick, which may not contain `@`; the server takes the machine from the connection's credential, or `server` for its own machine.
  `rosen change show` and `rosen review list --approvals` show `<nick>@<machine>`.
  Changes and approvals recorded before machines were kept show the bare nick.
- **Collapse action relay.**
  The project owner can approve, land and mark changes, open a change to `rosen.toml`, and revoke a lost machine, from Collapse, which cannot reach a server behind NAT, so the server fetches those requests and runs them.
  This is not a hosted server and not a web login (§6): Rosen stays the authority, and every request goes through the same checks as one from the CLI.
  - Only the operator turns it on, per server: `rosen server relay --allow approve,land,mark,config,revoke`, or `--allow none` to turn it off.
    Each kind is allowed only by name; `revoke` is never on unless named.
    The server refuses the setting from any machine but its own, and `rosen mcp` offers no tool for it.
    It lives in the metadata store, not in `rosen.toml`, because a change must not grant web authority.
    `rosen server status` shows it, and the state feed (R9) sends the allowed kinds as `relay`, so Collapse hides buttons the server won't honour.
  - Relayed actions run as the reserved machine `collapse`, which the machine list shows and which has no credential, so no peer can authenticate as it.
    The nick is the Collapse account the action carries, refused if it contains `@`, so approvals and marks read `<account>@collapse`.
    `rosen machine revoke collapse` turns the relay off as `--allow none` does; allowing a kind again re-enrolls it.
  - With a `collapse` root set (R9), the server reads `<collapse>/rosen/doorbell` every 20 s with the state feed's push token.
    Collapse's edge answers it with a generation number without waking Collapse's origin.
    Only when the generation changed, or a state feed answer says `actions_pending`, does the server read `<collapse>/rosen/actions?after=<id>`.
  - It runs only `approve` (`review approve` of the named head, so a stale head gets R7's refusal), `land` (every R8 gate) and `mark` (`review mark`, refused unless the change's current land is the named commit).
    Claims, messages, receipts, abandon, rebase, conflict resolution, machine and key commands other than `machine_revoke`, server settings and imports are never relayed.
    No relayed action creates a join key or approves a pending machine.
  - `config_change` (allowed as `config`) carries `base`, `expected_blob` (the base's `rosen.toml` blob the owner edited, or null when absent), `text` (the whole new file, at most 256 KiB) and `message`.
    It is refused, creating nothing, when `text` fails the server's configuration rules, with the parser's message, or when the current base's `rosen.toml` blob differs from `expected_blob`, with "rosen.toml changed on the base; reload".
    Otherwise it creates a change on that base owned by `<account>@collapse`, writes `rosen.toml` through a server-side workspace (R3), records a named snapshot with `message`, and answers with the change ID and head.
    It never approves or lands the change: R7 and R8 apply unchanged, and the configuration in force is read from the base, so the change cannot lower its own gates.
  - `machine_revoke` (allowed as `revoke`) carries `machine` and runs the same revoke as `rosen machine revoke`, so the machine's next request is refused and its changes, claims and workspaces are kept.
    It is refused, changing nothing, for `server`, for `collapse` (`rosen machine revoke collapse` on the server still works), for an unknown machine with `not_found`, and for an already revoked one.
    Its result is the revoked machine record with `by` set to `<account>@collapse`, and the next state feed pass sends the changed machine list without waiting for a quiet spell.
  - A kind not allowed, an action past its `expires_at` and a malformed one are refused and run nothing.
    The server stores each action ID it takes up, marked running before it runs, so a repeated ID runs nothing.
  - The outcome is stored, then posted to `<collapse>/rosen/actions/<id>/result` as `done` or `refused`: the `Approval`, the `LandAnswer` or the `LandedReview`, or a `rosen.error/v1` object with a `code`.
    A failed post is retried with the state feed's backoff, and a restarted server re-posts stored outcomes.
    An action a stopped server left running is settled at the next start: by the approval or mark it stored, if any, and otherwise by one run.
  - Each outcome, setting change and failure to reach Collapse publishes a `relay` event to every subscriber.
- **One enrollment for Rosen and Velocity.**
  Velocity's cross-system dispatch uses the same join key, so one command adds a machine to both (§9).
- **Accept when:** a headless machine joins from one command with no interactive step; a revoked machine's next request is refused; and a daemon that meets a certificate other than the key's fingerprint never sends the key.
