- **Load limits.**
  Each daemon caps its machine's concurrent materialisations, and the server caps server-side workspace I/O; both queue the rest.
  The client caps local disk use.
  A request over a cap waits; it never fails or degrades another workspace.
  Each daemon budgets buffered RPC bodies across connections, and limits concurrent memory-intensive requests through delivery of the reply.
