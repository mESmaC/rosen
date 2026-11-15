- Merge previews and landing attempts read commit trees directly from immutable objects.
  Preparing a merge does not populate the derived history index or take a SQLite write lock merely to read a commit's tree.
  Clean merge results are cached in bounded memory by the exact three input trees and policy commit, so a land can reuse its checker's merge without recomputing it.
  Receipts, approvals, conflicts and other mutable gates are never cached with the merge.
  Merge preparation limits the base-side diff to paths the candidate edits; final tree installation still detects file/directory collisions.
  Live attempts stage merge objects in at most 8 MiB of scratch memory and stop persisting them once their base moves; larger results spill to the durable store.
  Every object in a successful merge result is durable before receipt validation and the atomic ref move.
  Historical receipt previews keep their explicitly named base and are never cancelled by a newer base.
