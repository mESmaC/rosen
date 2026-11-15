- Each landing attempt reads the check-set name and computes the review tier from the configuration at that attempt's base, including retries after a lost compare-and-swap.
  The review mode is also read from that exact base, so a change cannot disable its own gates.
  Before-land prose-only changes still need approval and a clean merge.
