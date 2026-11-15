### R6 · Path claims and edit tracking
- `rosen claim <paths…>` records notices for a change and never refuses because of overlapping work, including an exclusive claim.
  Its reply and a durable `claim` event name every other live change claiming, sharing or editing those paths, including generated files and registration lists.
  The event reaches those changes' owners so both sides learn of concurrent work.
  - Notice tree diffs run outside both the metadata write transaction and the change registry lock.
    The grant transaction reads current live heads and parent inputs and uses only matching immutable diffs, retrying outside the locks for missing inputs.
    Retries reuse unchanged diffs without loading snapshot histories; claim/share matching and the durable event remain atomic with the grant (#378).
- `rosen claim --exclusive <paths…>` opts into first-claim-wins: an overlapping claim on another live change refuses the whole request and names its holders.
  It refuses any pattern that covers a generated file or registration list, using the base's configuration.
  Existing stored claims retain their exclusive mode on upgrade.
  `share` still records concurrent editing, and `wait` lets an agent choose to hold off.
- Edit regions update from each snapshot.
  Ignored paths (R2) have no edit regions.
  The server derives the live path and region view from one captured head and parent; reverting a path to its parent removes its regions.
  Regions use zero-based, half-open parent line spans, with an empty span for an insertion.
  The view survives restart through the stored snapshot objects and change metadata.
  `rosen-client` exposes it through `Client::edit_regions`, including the captured head and parent tree IDs.
- An **overlap query** returns, for a change or a set of paths, every change that overlaps it.
  The channel (R13) and Velocity call it.
  - A claim or share covers every line of the paths it matches, so it overlaps another change's claim, share or edit there.
    A wait does not, because a waiter holds off.
    A set of paths in a query counts as a claim.
    A query that names a change and paths returns only that change's overlaps on those paths.
  - Post-land conflict probes use edit-region overlaps only (R8 step 4).
    Claims and shares still receive overlap notices, without moving their changes.
  - Edits are measured against the change's parent.
    Two changes on different parents are compared in one parent's lines, after carrying the other's edits across the lines both versions share.
    An added, deleted, binary or mode-changed file counts as edited throughout.
- These are AIRC's file claims: `airc_claim` files, `airc_share` and `airc_wait` record path claims, shares and waits here (R13).
  There is one claim record per path, not one in AIRC and one in Rosen.
  CLI claim, share, wait, release and status output uses v2 schemas for the new mode and concurrent-work fields; the catalog retains v1 (R10).
- Claims release when their change lands or is abandoned.
  Abandonment and removal of its claims, shares and waits commit together.
  Release transactions retain pending notifications until published, so a restart replays a wait's wake even after its record was removed.
  Delivery is at least once.
  A wait is then signalled by an event.
- **Accept when:** two concurrent default claims on `SPEC.md` both succeed and each owner learns of the other; two exclusive claims on one path never both succeed; release wakes a waiter once no other live claim remains on its paths.
