### R10 · CLI
- The `rosen` commands are `import`, `mirror adopt|catch-up|drop-paths` (operator only, R1), `change new|list|show|abandon`, `commit`, `status`, `diff`, `log`, `claim|share|wait|release`, `overlaps`, `review comment|approve|resolve|list|mark|landed|failures`, `receipt submit`, `land`, `rebase`, `resolve`, `workspace list|delete|watch`, `server start|stop|status`, `run`, `config show`, `events`, `machine join|list|approve|revoke`, `machine key create|list|revoke` (R14) and `mcp`.
- `--json` on every command, with versioned output schemas.
  Each output line has a `schema` ID (`rosen.<shape>/v1`); incompatible shape changes bump that version.
  `--json-schema` prints the offline JSON Schema catalog (`crates/rosen-cli/schemas/v1.json`), including nested records and refusal variants.
  Usage and runtime failures, including retried workspace watcher failures, use `rosen.error/v1` with `command` (null for usage errors) and `error`.
  Help and version requests remain text.
- `--help` is complete enough that an agent can learn Rosen from it alone.
  When both `ROSEN_CHANGE` and `ROSEN_MANAGED_WORKSPACE` are set, help and the quick start adapt for a host-managed workspace: host-owned commands (`land`, `receipt`, `workspace delete`) are hidden, `rebase` drops "with its watcher stopped", and conflict messages list the conflicting paths and exact commands to run.
- `rosen status` reports `next_action`, the responsible `actor` (`agent`, `velocity`, `owner` or null when closed), `affected_head`, and for a stored conflict, `conflict` (`paths`, `base_applied` and `needs_naming_again`).
  Its help and MCP tool description document `tier`, `reach` and `recheck_mode`.
- `rosen commit` returns the post-commit file summary (`files`) and `land_check` in the same response.
- `rosen diff` flushes pending workspace edits before diffing the current workspace's change, and lists any files not yet snapshotted prominently.
- `rosen mcp` serves the commands as Model Context Protocol tools on stdin and stdout, for an agent host to start.
  Each tool is one command, named by its words joined with `_`, with the command's arguments and help as its inputs, read from the same definitions `--help` prints.
  A call runs that command with `--json` and returns its versioned JSON object as `structuredContent` alongside a text copy; a failure or refusal is an error result holding its `rosen.error/v1` object.
  `--workspace <DIR>` runs the commands in that workspace.
  `wait` (parking with `--park`), `events` (draining pending events with `--once`) and `run` (wrapped as `rosen.run/v1`) are offered as tools so MCP matches the CLI for agent use; `workspace watch` and the operator's commands (`import`, `mirror`, `server start`, `server stop`, `server relay`, `machine`) are not tools.
  `review approve`, `receipt submit` and `land` are tools only with `--landing`; in managed workspaces without `--landing`, `workspace delete` is also omitted.
- `rosen server stop` asks the server to stop and returns once its process has exited, which is when its data directory is free for another server.
  It never starts a server, and succeeds when none is running.
  A stopping server refuses new work, answers the requests it is already running, a land included, and then exits.
- `rosen server status` reports the git commit the server was built from and the path of its executable, since the version is the same for every commit.
