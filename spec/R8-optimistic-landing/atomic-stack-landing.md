3. A **stack** lands in one request: `rosen land <change>` includes all its unlanded ancestors and the change itself.
   The server writes one landed commit per change, chained bottom first, and moves the ref once by compare-and-swap.
   Before-land projects require approval on every layer and only one receipt, on the top's final merge result.
   `parent_unlanded` means a lower change cannot land (for example, it is abandoned, conflicted or unapproved), rather than requiring a separate land request.
