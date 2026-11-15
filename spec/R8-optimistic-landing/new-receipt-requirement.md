- otherwise in `reach` mode, with a clean merge → a new pre-land receipt on the new merge result is required.
  A clean textual merge can still break the build.
  For example, a field one change adds can break another change's struct literal in a file the first never touched.
  Those two changes are not independent, because the second change's unit depends on the first's;
