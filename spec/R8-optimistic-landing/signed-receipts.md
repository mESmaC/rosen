- **Signed receipts.**
  Every machine's daemon holds an Ed25519 receipt key, created on first use in its data directory, readable by the current OS user only, and never replaced.
  It signs each receipt a client on that machine submits, over the change ID, head, base, check set and submitter.
  The server takes the submitting machine from the connection's machine credential, never from the request, and refuses an unsigned receipt from the network.
  The request names only the submitter's nick, which may not contain `@`.
  The server records the submitter as nick plus machine: the enrolled machine's id, or `server` for the server's own machine.
  The stored receipt `rosen receipt submit` returns and the landed commit's trailer both show it.
  A joining daemon sends its public key with its enrollment or pending request, and the server binds it to the machine in the same transaction that issues the credential.
  A machine enrolled without one, before daemons sent it, has its key bound the first time the server accepts a signed receipt from it.
  Either way, a receipt signed with any other key is refused.
  A receipt submitted on the server's own machine is signed with the server's key.
  The server verifies the signature when it stores the receipt and again whenever a land decides on it: a receipt whose fields no longer match what was signed, or whose machine has since been revoked, does not count, and the change needs a new receipt.
