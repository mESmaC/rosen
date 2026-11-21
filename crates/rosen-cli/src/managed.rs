//! Managed-workspace mode (SPEC R10): when an external host such as Velocity
//! owns the workspace lifecycle, watcher, checks, review and landing, CLI help
//! and MCP tools hide host-owned commands and tailor conflict instructions.

use clap::Command;

/// Environment variable naming the agent's active change.
pub const CHANGE_ENV: &str = "ROSEN_CHANGE";

/// Environment variable marking a workspace managed by an external host.
/// Any non-empty value other than `0` or `false` enables managed mode when
/// `ROSEN_CHANGE` is also set.
pub const MANAGED_ENV: &str = "ROSEN_MANAGED_WORKSPACE";

/// True when both `ROSEN_CHANGE` and `ROSEN_MANAGED_WORKSPACE` are set.
pub fn is_managed() -> bool {
    let has_change = std::env::var(CHANGE_ENV)
        .ok()
        .is_some_and(|v| !v.trim().is_empty());
    let marked = std::env::var(MANAGED_ENV).ok().is_some_and(|v| {
        let trimmed = v.trim();
        !trimmed.is_empty()
            && !trimmed.eq_ignore_ascii_case("0")
            && !trimmed.eq_ignore_ascii_case("false")
    });
    has_change && marked
}

const MANAGED_LONG_ABOUT: &str = "\
Rosen: version control for many coding agents working at once.

Your workspace and change ($ROSEN_CHANGE) are managed by your host: writes
are snapshotted to the Rosen server within 2 s, and the host runs checks,
review, landing and workspace cleanup.

Typical flow: `rosen claim <paths>`, edit files, inspect with `rosen status`
or `rosen diff`, then name your work with `rosen commit -m <message>`.";

const MANAGED_AFTER_LONG_HELP: &str = "\
Getting started (managed workspace):
  rosen status                  your change, next action, claims and conflicts
  rosen claim <PATHS>           claim or notice what you will edit
  rosen overlaps                check concurrent work on your paths
  rosen diff                    diff your change (flushes pending edits first)
  rosen rebase                  apply a server rebase or get conflict markers
  rosen resolve <FILES>         clear conflicts after editing out markers
  rosen commit -m <MESSAGE>     name your work (returns file summary and land check)
  rosen run -- <COMMAND>        run a build or test in its turn in the run queue

Changes are named by their ID (32 letters k-z) or any unique prefix of it.
Host-owned commands (`land`, `receipt`, `workspace delete`) are hidden in
managed workspaces because your managing client owns landing and cleanup.

Environment:
  ROSEN_AGENT              your agent identity (owner, author, submitter)
  ROSEN_CHANGE             your managed change ID (used when --change is omitted)
  ROSEN_MANAGED_WORKSPACE  set by the host to mark a managed workspace
  ROSEN_ENDPOINT           the daemon's pipe name or socket path

Output:
  Text is for people. With --json each command prints one JSON object per
  line whose `schema` field names its shape and version, such as
  `rosen.status/v2`. A shape change bumps the version. Failures print
  `rosen.error/v1` objects with `command` and `error` fields.
  Use --json-schema to print the complete validation catalog offline.
  Help and version requests print text even with --json.

Exit status:
  0  the command did what it was asked
  1  it failed or was refused (a claim held by another change, a server error)
  2  the command line was invalid";

const MANAGED_REBASE_LONG_ABOUT: &str = "\
Rebase a change onto the current base and write it into its workspace.

The server merges the change's edits onto the base. A file where both
changed the same lines gets diff3 conflict markers (`<<<<<<< ours` is
the change's side, `>>>>>>> theirs` the base's) and an open conflict.
Combine both sides between the markers, then run `rosen resolve <files>`
and name the change again with `rosen commit -m \"<message>\"`.

Run inside the change's workspace, this stores a snapshot first and
then writes the rebased files into the workspace; the managing client
coordinates its watcher. With --change, or $ROSEN_CHANGE outside a
workspace, only the server rebases; the workspace takes the result at
its next `rosen rebase` there. A stacked change moves when its parent
lands.";

/// Adapts the clap command tree for managed workspaces: hides host-owned
/// commands (`land`, `receipt`, `workspace delete`), swaps the quick start,
/// and updates `rebase` help to match host-coordinated watching.
pub fn configure_command(cmd: Command, managed: bool) -> Command {
    if !managed {
        return cmd;
    }
    cmd.long_about(MANAGED_LONG_ABOUT)
        .after_long_help(MANAGED_AFTER_LONG_HELP)
        .mut_subcommand("land", |sub| {
            sub.hide(true)
                .about("[Managed by host] Land a change onto the base.")
        })
        .mut_subcommand("receipt", |sub| {
            sub.hide(true)
                .about("[Managed by host] Submit check results for a change's merge result.")
        })
        .mut_subcommand("workspace", |sub| {
            sub.about("List workspaces or inspect materialisation limits.")
                .mut_subcommand("delete", |del| {
                    del.hide(true)
                        .about("[Managed by host] Delete a workspace.")
                })
        })
        .mut_subcommand("rebase", |sub| sub.long_about(MANAGED_REBASE_LONG_ABOUT))
}

/// Quotes a path for a shell command line only when needed.
pub fn shell_quote(path: &str) -> String {
    let safe = !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b':'));
    if safe {
        path.to_owned()
    } else {
        format!("\"{}\"", path.replace('"', "\\\""))
    }
}

/// Exact commands to resolve open conflicts on a change.
pub fn conflict_commands(
    paths: &[String],
    base_applied: Option<bool>,
    needs_naming_again: bool,
    change_id: &str,
    managed: bool,
) -> Vec<String> {
    let mut commands = Vec::new();
    if base_applied == Some(false) {
        commands.push("rosen rebase".to_owned());
    }
    let files = if paths.is_empty() {
        "<file>".to_owned()
    } else {
        paths
            .iter()
            .map(|p| shell_quote(p))
            .collect::<Vec<_>>()
            .join(" ")
    };
    commands.push(format!("rosen resolve {files}"));
    if managed {
        if needs_naming_again {
            commands.push("rosen commit -m \"<message>\"".to_owned());
        }
    } else {
        commands.push(format!("rosen land {change_id}"));
    }
    commands
}
