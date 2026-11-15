## Proof list

These features are proven in Lean after their code (F17).
Every other feature ships with no Lean.

- Optimistic landing and auto-revert (R8, R12): decides when the base moves; a wrong compare-and-swap, receipt binding, re-check rule, stack order or culprit disposition bypasses configured gates, applies a land twice, reverts a test-only failure after land or the wrong change, or drops another change's work
- Path and piece claims (R6, R13): piece claims and opt-in exclusive path claims use first-claim-wins; default path notices never refuse; a wrong rule serializes authors unnecessarily, grants two exclusive holders, or strands a waiter
- Workspace deletion guard (R4): must never delete an edit not yet stored on the server, across the snapshot window and the offline journal; a wrong guard loses an agent's work
- Three-way merge (R5): every land and rebase merges automatically; a wrong merge silently drops or duplicates lines
