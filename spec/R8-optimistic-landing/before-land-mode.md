1. **Before-land mode:** a checker gets the server-computed merge result of a change onto base commit B, runs the project's **pre-land check set** (build and lint, D20), and submits a **receipt** for `(head, B, pre-land check set)`.
   A wholly prose-only stack (R7 tier) needs no receipt.
   The server stores the checked merge-result tree with each receipt, retaining the preview shown for that head and base so a concurrent parent update cannot substitute a different tree.
   A late receipt may name the older head shown in that recorded preview; storing it keeps its original checked tree.
   A preview that would bind a different tree to an already recorded head/base pair is refused until parent propagation or a rebase advances the head.
