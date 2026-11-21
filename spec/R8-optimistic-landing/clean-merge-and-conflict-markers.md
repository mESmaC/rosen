- every change in the requested stack has no conflicts and merges cleanly.
  When a layer needs generated-file regeneration and the base's `rosen.toml` registers commands under `[generated.commands]` for every such path, the server materialises the merged input tree, runs the commands in the run queue (`landing` class), and applies only those regenerated paths onto the merged tree before continuing (#428).
  If any registered command fails, the land is refused with `regeneration_failed`, naming the change, exact paths, merged input tree, registered commands and command output log; no prefix lands.
  If any path needing regeneration has no registered command, the land is refused with `regeneration_required`, naming the owning change, exact paths, merged input tree and any registered commands, in either review mode; no prefix lands.
  Its owner applies a rebase, regenerates the reported paths on the merged inputs and snapshots the repaired head before retrying.
  A provisional merge tree awaiting regeneration cannot be recorded as a checked preview or certified by a receipt.
  Before writing each landed commit, the server also refuses any file in its final tree containing conflict marker lines, even if every conflict row was resolved or no row exists.
  Marker lines start with at least seven `<`, `|` or `>` characters followed by whitespace or the line end, or consist of at least seven `=` characters; both LF and CRLF line endings are recognized.
  Blob reads stream, including large files.
  Bounded server-local memory remembers marker-free blob and subtree IDs only after their scans succeed, so unchanged objects need no rescan on another land attempt.
  In `after-land` mode it needs no approval or receipt, of any tier; the server writes the landed commit and compare-and-swaps the base it merged onto.
  It skips receipt lookup and all receipt reuse/re-check rules;
