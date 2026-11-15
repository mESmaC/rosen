- every change in the requested stack has no conflicts and merges cleanly.
  A layer that requires generated-file regeneration refuses the entire land with `regeneration_required`, its owning change and paths, in either review mode; no prefix lands.
  Its owner applies a rebase, regenerates the reported paths on the merged inputs and snapshots the repaired head before retrying.
  A provisional merge tree awaiting regeneration cannot be recorded as a checked preview or certified by a receipt.
  Before writing each landed commit, the server also refuses any file in its final tree containing conflict marker lines, even if every conflict row was resolved or no row exists.
  Marker lines start with at least seven `<`, `|` or `>` characters followed by whitespace or the line end, or consist of at least seven `=` characters; both LF and CRLF line endings are recognized.
  Blob reads stream, including large files.
  Bounded server-local memory remembers marker-free blob and subtree IDs only after their scans succeed, so unchanged objects need no rescan on another land attempt.
  In `after-land` mode it needs no approval or receipt, of any tier; the server writes the landed commit and compare-and-swaps the base it merged onto.
  It skips receipt lookup and all receipt reuse/re-check rules;
