- the requester is answered once the transaction that moves the ref commits, the `landed` event is published and the changes stacked on the landed change are rebased, so a checker that asks for a child's merge result next sees the child on the new base.
  That transaction also records the land's post-land work as pending.
  The rest of the work below, which is releasing claims, the `released` event and probing overlapping edits, can follow the answer, and recovery finishes it if the server stops first;
