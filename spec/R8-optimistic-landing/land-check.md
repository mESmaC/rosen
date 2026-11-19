- `rosen land --check <change>` (`Request::LandCheck`) reports every gate a land of the change would apply on the current base, under the configuration at that base, and lands nothing.
  It answers the `review` and `recheck` modes, the review tier the land would record, the change's build-unit reach (the unit names, or `all`), the base and the head.
  It gives one verdict per gate: `merge` (`clean`, `conflicts`, `merge_conflict`, `regeneration_required`, `oversize_files`, `conflict_markers` or `not_checked`), `approval` (`held`, `needed` or `not_required`), `receipt` (`valid`, `reusable`, `stale`, `unbound`, `missing`, `not_required` or `not_checked`), and `stack`, the unlanded ancestors an atomic land would include.
  With a clean merge, `line_endings` lists each file the merge result holds in other line endings than the change gave it, with the newline it lands with (R5).
  `landable` is true when a land on that base would commit, barring a lost compare-and-swap; otherwise `refusal` carries the reason, message and paths the land would answer with, in the land's own order.
  The check reads the land's inputs through the land's own functions and decides with the same rule, so a land that follows on the same base never disagrees with it.
  It changes nothing: it never marks the change ready, routes a merge conflict, publishes an event or stores a receipt.
  It reads through the read-only connections of [Ledger lookups](ledger-lookups.md), never under the land writer.
  `rosen status` shows the tier and the gate summary for an open or ready change.
