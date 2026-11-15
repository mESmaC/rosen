- every change in the requested stack has no conflicts and merges cleanly.
  Before writing each landed commit, the server also refuses any file in its final tree containing conflict marker lines, even if every conflict row was resolved or no row exists.
  Marker lines start with at least seven `<`, `|` or `>` characters followed by whitespace or the line end, or consist of at least seven `=` characters; both LF and CRLF line endings are recognized.
  Blob reads stream, including large files.
  In `after-land` mode it needs no approval or receipt, of any tier; the server writes the landed commit and compare-and-swaps the base it merged onto.
  It skips receipt lookup and all receipt reuse/re-check rules;
