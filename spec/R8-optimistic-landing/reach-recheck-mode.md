- with `recheck = "reach"` (default), every change landed since B is **independent** of this change (§2), and the merge is clean → the existing receipt still counts.
  Each unit this change reaches has the same inputs the receipt checked, and every other unit is as the landed changes' own receipts left it;
