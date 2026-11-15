- Post-land work that a crash or a failed step interrupts is replayed: at server start, and then with backoff whenever a land's post-land work fails.
  One land's failure never holds back another's.
  A land's post-land work runs at most once at a time, and a replay publishes the land's `landed` and `released` events only if the log does not hold them yet.
  A rebase only moves a change forward, and only from the head its merge read: a change already on the landed commit or a later base is left there, so a replayed or slower post-land run never moves it behind a newer base, and a snapshot stored meanwhile is merged again rather than overwritten.
