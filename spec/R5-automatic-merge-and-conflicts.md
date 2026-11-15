### R5 · Automatic merge and conflicts
- The server merges three ways at file level and line level, against the merge base.
- Text merges ignore a CR immediately before LF when comparing lines, including registration lists and file-level unchanged-side shortcuts.
  Merged text uses the base's first terminated line's newline convention; if the base has none, it uses ours' first terminated line, then theirs', otherwise LF.
  Bare CR bytes and missing final newlines remain content; generated and binary files retain their existing byte-preserving rules.
- Generated files listed in project configuration are regenerated, never merged.
  When both sides change a generated path differently, the provisional merge keeps the target base's copy and reports the path for the owner to regenerate; Rosen runs no generators (D9).
  Server rebases preserve these paths in the owner's rebase notice, including restacks after a parent lands.
  Registration lists keep both sides' additions.
- A region that does not merge automatically becomes a stored conflict on the change, which records base, ours and theirs.
  A local workspace shows conflict markers once the rebase is applied (R2).
  The markers are written as `git merge-file --diff3` writes them, in the file's line endings, with the change's side as `ours` and the base's as `theirs`.
  A rebase that conflicts, such as after a land refused for a conflict, stores the conflicts and gives the change a head on the new base whose conflicting files hold the markers, so the owner can keep both sides.
  Open conflicts never block a rebase, and a rebase never clears one.
  Snapshots replace already conflicted files directly (R2); `rosen resolve` clears their conflicts only after checking that the stored files contain no conflict markers, refusing all requested paths together if any still has markers.
- **Accept when:** a clean merge never drops or duplicates a line that either side kept, and every non-clean region becomes a conflict instead of a silent pick.
