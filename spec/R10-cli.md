### R10 · CLI
- The `rosen` commands are `import`, `mirror adopt|drop-paths` (operator only, R1), `change new|list|show|abandon`, `commit`, `status`, `diff`, `log`, `claim|share|wait|release`, `overlaps`, `review comment|approve|resolve|list|mark|landed|failures`, `receipt submit`, `land`, `rebase`, `resolve`, `workspace list|delete|watch`, `server start|status`, `run`, `config show`, `events`, `machine join|list|approve|revoke`, `machine key create|list|revoke` (R14) and `mcp`.
- `--json` on every command, with versioned output schemas.
  Each output line has a `schema` ID (`rosen.<shape>/v1`); incompatible shape changes bump that version.
  `--json-schema` prints the offline JSON Schema catalog (`crates/rosen-cli/schemas/v1.json`), including nested records and refusal variants.
  Usage and runtime failures, including retried workspace watcher failures, use `rosen.error/v1` with `command` (null for usage errors) and `error`.
  Help and version requests remain text.
- `--help` is complete enough that an agent can learn Rosen from it alone.
- `rosen mcp` serves the commands as Model Context Protocol tools on stdin and stdout, for an agent host to start.
  Each tool is one command, named by its words joined with `_`, with the command's arguments and help as its inputs, read from the same definitions `--help` prints.
  A call runs that command with `--json` and returns its output; a failure or refusal is an error result holding its `rosen.error/v1` object.
  `--workspace <DIR>` runs the commands in that workspace.
  Commands that run until something happens elsewhere (`wait`, `events`, `workspace watch`) and the operator's commands (`import`, `mirror`, `server start`, `machine`) are not tools.
  `review approve`, `receipt submit` and `land` are tools only with `--landing`.
