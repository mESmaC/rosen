### R5 · Automatic merge and conflicts
- The server merges three ways at file level and line level, against the merge base.
- Text merges ignore a CR immediately before LF when comparing lines, including registration lists and file-level unchanged-side shortcuts.
  Merged text uses the base's first terminated line's newline convention; if the base has none, it uses ours' first terminated line, then theirs', otherwise LF.
  Bare CR bytes and missing final newlines remain content; generated and binary files retain their existing byte-preserving rules.
- A change's merge result, which checkers build and lands write, follows the target base's declared newlines, even when nothing landed under the change and no merge runs (#397, #424).
  When the base's `.gitattributes` gives a text file the change adds or edits `eol=lf` or `eol=crlf`, the file is written in that form, so a land repairs a file stored in the other form and undoes an editor's rewrite of it.
  Attributes are read as git reads them: the root's file first, then each directory's down to the path, with the last matching line winning; `binary`, `-text` and the legacy `crlf` forms count, and patterns with quotes, character classes or escapes are ignored.
  A file with no declared `eol` is written in the convention of the base's version of that path, read from its first terminated line.
  A new undeclared file, or one whose base version has no terminated line, takes the convention most of the base's text files use, sampled from a bounded number of them.
  Binary files, files marked `-text` or `binary`, generated, symlink and gitlink entries, and files over 16 MiB, keep their bytes.
  `rosen land --check` and the land answer list each file the merge result holds in other line endings than the change gave it, with the newline it lands with.
  Reverts, restacks and snapshot merges keep every byte; Rosen reads no `core.autocrlf`.
- Generated files listed in project configuration are regenerated, never merged.
  When both sides change a generated path differently, the provisional merge keeps the target base's copy and reports the path for regeneration.
  If the base's `rosen.toml` registers commands under `[generated.commands]` for every path needing regeneration, the server runs them on the merged tree in the run queue during `merge-result` and `land` and applies only those regenerated paths onto the merged tree (#428).
  Otherwise (or if a command fails), the report names the exact paths, the merged input tree, any registered commands and (on failure) the command log.
  Server rebases preserve these paths (and any registered commands) in the owner's rebase notice, including restacks after a parent lands.
  Registration lists keep both sides' additions.
- A region that does not merge automatically becomes a stored conflict on the change, which records base, ours and theirs.
  A local workspace shows conflict markers once the rebase is applied (R2).
  The markers are written as `git merge-file --diff3` writes them, in the file's line endings, with the change's side as `ours` and the base's as `theirs`.
  A rebase that conflicts, such as after a land refused for a conflict, stores the conflicts and gives the change a head on the new base whose conflicting files hold the markers, so the owner can keep both sides.
  Open conflicts never block a rebase, and a rebase never clears one.
  Snapshots replace already conflicted files directly (R2); `rosen resolve` clears their conflicts only after checking that the stored files contain no conflict markers, refusing all requested paths together if any still has markers.
- **Accept when:** a clean merge never drops or duplicates a line that either side kept, and every non-clean region becomes a conflict instead of a silent pick.
