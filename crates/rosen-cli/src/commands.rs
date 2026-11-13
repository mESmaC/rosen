//! The `rosen` command tree (SPEC.md R10).
//!
//! Help text is written for agents: SPEC.md requires that `--help` alone is
//! enough to learn Rosen, so every command and argument says what it does
//! and when to use it.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The part of `rosen --help` an agent needs before its first command:
/// the workflow, identity, output and exit conventions (SPEC.md R10).
const AFTER_LONG_HELP: &str = "\
Getting started:
  rosen server start                      start this machine's daemon (most
                                          commands start it on demand)
  rosen change new --workspace <DIR>      new change with a local workspace
  rosen workspace watch <DIR>             snapshot every write within 2 s
  rosen claim --change <CHANGE> <PATHS>   claim what you will edit
  rosen status                            your change, claims, conflicts and
                                          unsnapshotted files
  rosen commit -m <MESSAGE>               a named snapshot
  rosen review approve <CHANGE> --head <HEAD>  approve the head you read
  rosen receipt submit <CHANGE> ...       (a checker) record passed checks
  rosen land <CHANGE>                     move the base to the change

Changes are named by their ID (32 letters k-z) or any unique prefix of it.
A workspace is any directory `rosen change new --workspace` made; commands
run inside one use its change when no change is given.

Environment:
  ROSEN_AGENT     your agent identity (owner, author, submitter); defaults
                  to the OS user name
  ROSEN_CHANGE    the change `claim`, `share`, `wait`, `release` and
                  `status` act on when no --change is given
  ROSEN_ENDPOINT  the daemon's pipe name or socket path

Output:
  Text is for people. With --json each command prints one JSON object per
  line whose `schema` field names its shape and version, such as
  `rosen.change/v1`. A shape change bumps the version. Failures print
  `rosen.error/v1` objects with `command` and `error` fields.
  Use --json-schema to print the complete validation catalog offline.
  Help and version requests print text even with --json.

Exit status:
  0  the command did what it was asked
  1  it failed or was refused (a claim held by another change, a server
     that is not running for `server status`, a server error)
  2  the command line was invalid";

/// Rosen: version control for many coding agents working at once.
///
/// Each agent works on its own change, in its own workspace. Every write to
/// a workspace is snapshotted to the Rosen server within seconds, so there is
/// no push step. The server merges, rebases and lands changes; a land moves
/// the base ref by compare-and-swap, with no queue or lock.
///
/// Typical flow: `rosen change new`, edit files, `rosen claim <paths>`,
/// get a review and a receipt, then `rosen land <change>`.
#[derive(Debug, Parser)]
#[command(
    name = "rosen",
    version,
    propagate_version = true,
    after_long_help = AFTER_LONG_HELP
)]
pub struct Cli {
    /// Print machine-readable JSON with a versioned schema instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Print the complete JSON Schema catalog offline and exit.
    #[arg(long, global = true)]
    pub json_schema: bool,

    /// Server pipe name (Windows) or socket path. Defaults to $ROSEN_ENDPOINT,
    /// then the per-user default, so normally you never set it.
    #[arg(long, global = true, value_name = "ENDPOINT")]
    pub endpoint: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Import a git repository; its default branch becomes the base.
    ///
    /// Copies every commit, tree and blob reachable from the branch into the
    /// server's store with their git ids unchanged, then sets the base. Works
    /// only on a project with no base yet; rerunning after an interrupted
    /// import skips the objects already stored.
    Import {
        /// Path (bare or with a working tree) or URL of the git repository.
        repo: String,
        /// Branch to import instead of the one the repository's HEAD names.
        #[arg(long)]
        branch: Option<String>,
        /// Git remote (URL or path) to mirror the base to. After each land
        /// the server pushes the base there as a fast-forward, never a
        /// force-push, and retries a failed push in the background,
        /// reporting each failure as a `mirror` event.
        #[arg(long, value_name = "REMOTE")]
        mirror: Option<String>,
    },
    /// Create, list, inspect or abandon changes.
    #[command(subcommand)]
    Change(ChangeCommand),
    /// Record a named snapshot of the current workspace.
    ///
    /// While `rosen workspace watch` (or Velocity) watches a workspace, its
    /// writes are snapshotted within 2 s; commit to give a point in the
    /// change's history a message. Commit scans the whole workspace and
    /// sends only files that differ from the last snapshot, skipping paths
    /// the workspace's .gitignore files ignore unless already tracked.
    Commit {
        /// Message describing the snapshot.
        #[arg(short, long)]
        message: String,
        /// Workspace to snapshot. Defaults to the one holding the current
        /// directory.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
    },
    /// Show a change: its state, head, conflicts and path claims, and for a
    /// local workspace, what is not yet stored on the server.
    ///
    /// Inside a workspace (or with --workspace) it also scans every file,
    /// as `rosen workspace delete` would, and lists files that differ from
    /// the last stored snapshot, snapshots still waiting in the offline
    /// journal, and whether the server has rebased the change since this
    /// workspace's last snapshot (apply it before relying on line numbers).
    /// Reads only: it never snapshots or changes a file.
    Status(StatusArgs),
    /// Show a change's diff against its parent, as the server holds it.
    ///
    /// The server diffs the change's head, the tree of its latest snapshot
    /// (or of its merge onto a new base after a rebase), against the base
    /// commit or change it sits on. Edits made in the last 2 s may not be
    /// snapshotted yet. Needs no workspace: `--change` diffs any change.
    Diff(ChangePaths),
    /// Show history of the base, a commit or a change, or of some paths.
    ///
    /// One line per commit, newest first: commit ID, date (UTC), summary,
    /// and the change it landed or belongs to. History follows first
    /// parents. A change's log is its own snapshots, then the history of
    /// what it sits on. With paths, only commits that changed a file at or
    /// under one of them are shown (path history).
    Log {
        /// Change ID or prefix, a 40-hex commit ID, or `base`; defaults to
        /// the base.
        #[arg(long)]
        change: Option<String>,
        /// Limit history to these files or directories.
        paths: Vec<PathBuf>,
        /// Show at most this many entries (default 100).
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },
    /// Claim paths or globs for a change. The first claim wins; a refusal
    /// names the holder and exits with status 1.
    ///
    /// Two claims conflict when some path matches both, so `src/*.rs`
    /// conflicts with `src/lib.rs` but not with `src/*.md`. All paths are
    /// granted or none. Claims are released when the change lands or is
    /// abandoned, or by `rosen release`.
    Claim(ClaimArgs),
    /// Record that a change edits paths another change has claimed. A share
    /// is never refused; it tells the holder its path is edited knowingly.
    Share(ClaimArgs),
    /// Wait until no other change claims the paths, then exit.
    ///
    /// Records the wait and blocks. A `released` event for your change wakes
    /// it when the holder lands, is abandoned or releases the paths. Exits at
    /// once for paths nobody claims.
    Wait(ClaimArgs),
    /// Drop a change's claims, shares and waits on paths, waking any change
    /// waiting on them. `--all` drops every record the change holds.
    Release(ReleaseArgs),
    /// List changes that overlap a change or a set of paths.
    ///
    /// Two changes overlap when they claim the same path, or their edits in
    /// one file touch or sit within 3 lines of each other. A claim or share
    /// covers every line of its paths; a wait covers none. Generated files
    /// and registration lists never count. Only open and ready changes are
    /// listed. Paths given without `--change` are queried as a claim.
    Overlaps(OverlapArgs),
    /// Comment on, list or resolve review threads, or approve a change.
    #[command(subcommand)]
    Review(ReviewCommand),
    /// Submit check results for a change's merge result.
    #[command(subcommand)]
    Receipt(ReceiptCommand),
    /// Land a change onto the base.
    ///
    /// Needs no conflicts, an approval for the current head, and a receipt
    /// for the merge result (except prose-only changes). A stack lands
    /// bottom first.
    Land {
        /// Change to land.
        change: String,
    },
    /// Mark conflicts in paths as resolved after editing the conflict markers.
    Resolve {
        /// Paths whose conflicts are resolved.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// List or delete workspaces.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// Start or inspect this machine's Rosen server.
    #[command(subcommand)]
    Server(ServerCommand),
    /// Read the project configuration: the `rosen.toml` file at the
    /// repository root, as the base holds it.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Stream the events that concern you: snapshot, landed, rebased,
    /// conflict, updated, review, claim, released, green, reverted and mirror.
    ///
    /// You get an event when it is about a change you own, review, or stack
    /// on, or one whose claims or edits overlap yours; `mirror` events go to
    /// everyone. Each event carries a sequence number. The server stores how far you
    /// have read, once each event is printed, so rerunning `rosen events`
    /// resumes where you stopped, even after a server restart. Delivery is at
    /// least once: after a crash you may see an event again, never miss one.
    Events {
        /// Resume after this sequence number instead of your stored cursor.
        #[arg(long, value_name = "SEQ")]
        after: Option<u64>,
        /// The agent to stream for. Defaults to $ROSEN_AGENT, then the OS
        /// user name.
        #[arg(long = "as", value_name = "AGENT", conflicts_with = "all")]
        agent: Option<String>,
        /// Stream every event in the project from now (or --after), without
        /// storing a cursor. For dashboards and the Velocity view.
        #[arg(long)]
        all: bool,
    },
    /// Enroll this machine in a project, or list, approve and revoke the
    /// machines enrolled in it.
    ///
    /// One server holds each project; every other machine running agents
    /// for it is enrolled once with a join key, and every agent on it works
    /// under that machine's credential.
    #[command(subcommand)]
    Machine(MachineCommand),
}

#[derive(Debug, Subcommand)]
pub enum MachineCommand {
    /// Join with a key, or request pending admission with --address and
    /// --fingerprint. Non-interactive, for install scripts or machine images.
    ///
    /// The daemon connects to the server the key names (or finds it by
    /// DNS-SD on the local network) and checks the server's certificate
    /// against the key's fingerprint. On a mismatch it stops before sending
    /// the key. Otherwise it exchanges the key for this machine's own
    /// credential, stores it encrypted (DPAPI on Windows) and rotates it
    /// before it expires. The key is never used again by this machine.
    Join {
        /// The join key, as `rosen machine key create` printed it.
        key: Option<String>,
        /// Server address for a pending join without a key.
        #[arg(long, requires = "fingerprint", conflicts_with = "key")]
        address: Option<String>,
        /// Trusted server certificate fingerprint for a pending join.
        #[arg(long, requires = "address", conflicts_with = "key")]
        fingerprint: Option<String>,
        /// Machine name; defaults to the host name.
        #[arg(long)]
        name: Option<String>,
        /// Local daemon data directory holding the encrypted credential.
        #[arg(long, default_value = ".rosen")]
        data_dir: PathBuf,
    },
    /// List enrolled and pending machines with their role, last contact and
    /// running agents.
    List,
    /// Admit a machine that asked to join without a key and is pending.
    Approve {
        /// The machine, as `rosen machine list` names it.
        machine: String,
    },
    /// Revoke a machine's credential. Its next request is refused. Its
    /// changes, claims and workspaces are kept.
    Revoke {
        /// The machine, as `rosen machine list` names it.
        machine: String,
    },
    /// Create, list or revoke join keys.
    #[command(subcommand)]
    Key(MachineKeyCommand),
}

#[derive(Debug, Subcommand)]
pub enum MachineKeyCommand {
    /// Create a join key and print it. Anyone holding it can enroll
    /// machines until it expires or is used up, so pass it only to the
    /// machines' provisioning.
    ///
    /// The key carries the server's address and certificate fingerprint,
    /// so a joining machine needs nothing else.
    Create {
        /// Role of the machines it enrolls: `agent` runs agents, `checker`
        /// runs checks and submits receipts.
        #[arg(long, value_parser = ["agent", "checker"])]
        role: String,
        /// How long the key stays valid, such as `30m`, `24h` or `7d`.
        #[arg(long, value_name = "DURATION")]
        expires: String,
        /// How many machines can join with it.
        #[arg(long, value_name = "N")]
        uses: u32,
        /// Reachable server address to put in the key; defaults to its listener.
        #[arg(long)]
        address: Option<String>,
    },
    /// List join keys with their role, expiry and remaining uses. Never
    /// prints the secret part of a key.
    List,
    /// Revoke a join key so no further machine can join with it. Machines
    /// that already joined keep their own credentials.
    Revoke {
        /// The key's ID, as `rosen machine key list` prints it.
        key: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChangeCommand {
    /// Create a change and its workspace on the current base.
    ///
    /// Prints the new change's ID: 32 letters in k-z, fixed for the change's
    /// whole life (rebases, snapshots and lands never alter it). Any unique
    /// prefix of an ID works wherever a CHANGE is asked for.
    New {
        /// Stack the new change on this unlanded change instead of the base.
        #[arg(long, value_name = "CHANGE")]
        on: Option<String>,
        /// Agent that owns the change. Defaults to $ROSEN_AGENT, then the OS
        /// user name.
        #[arg(long, value_name = "AGENT")]
        owner: Option<String>,
        /// Write the change's files into this new directory. On a ReFS
        /// volume (a Dev Drive) files are block clones, which cost almost
        /// nothing; elsewhere they are plain copies. Files are never
        /// hardlinked, so writing one never affects another workspace.
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        /// With --workspace, hold only this path and what lies under it.
        /// Repeat for several paths. Default: the whole tree.
        #[arg(long, value_name = "PATH", requires = "workspace")]
        sparse: Vec<String>,
    },
    /// List changes and their states (open, ready, landed, abandoned),
    /// oldest first.
    List {
        /// Only changes owned by this agent.
        #[arg(long, value_name = "AGENT")]
        owner: Option<String>,
        /// Only changes in this state: open, ready, landed or abandoned.
        #[arg(long)]
        state: Option<String>,
    },
    /// Show one change: owner, parent, head, state, snapshots and conflicts.
    Show {
        /// Change to show: its ID or a unique prefix.
        change: String,
    },
    /// Abandon an open or ready change. Its claims are released and its
    /// workspace deleted.
    Abandon {
        /// Change to abandon: its ID or a unique prefix.
        change: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// Start a review thread on a line of a change, or reply in a thread.
    ///
    /// A new thread anchors to --path and --line at the change's current
    /// head, and the path must be a file there with that line. The anchor
    /// never moves, even when the author edits the file later. A reply
    /// (--thread) to a resolved thread reopens it. Prints the whole thread.
    /// Only open and ready changes take comments.
    Comment {
        /// Change to comment on: its ID or a unique prefix.
        change: String,
        /// File the new thread anchors to, relative to the repository root.
        #[arg(long, required_unless_present = "thread", conflicts_with = "thread")]
        path: Option<String>,
        /// Line (from 1) of that file the new thread anchors to.
        #[arg(long, required_unless_present = "thread", conflicts_with = "thread")]
        line: Option<u32>,
        /// Comment text.
        #[arg(short, long)]
        message: String,
        /// Reply in this thread instead of starting a new one.
        #[arg(long, value_name = "THREAD")]
        thread: Option<u64>,
        /// The head tree you read (from `rosen change show`). The comment is
        /// refused if the change has a different head now, because its line
        /// numbers may have shifted. Default: whatever head it has.
        #[arg(long, value_name = "TREE")]
        head: Option<String>,
        /// Who is commenting. Defaults to $ROSEN_AGENT, then the OS user name.
        #[arg(long, value_name = "AGENT")]
        author: Option<String>,
    },
    /// Approve a change at its current head. A new head by its author voids
    /// the approval; a server rebase keeps it.
    Approve {
        /// Change to approve.
        change: String,
        /// Exact head read by the reviewer.
        #[arg(long)]
        head: String,
        /// Reviewer identity. Defaults to $ROSEN_AGENT, then the OS user name.
        #[arg(long)]
        reviewer: Option<String>,
    },
    /// Mark a review thread resolved. Resolving a resolved thread does
    /// nothing; a reply reopens it.
    Resolve {
        /// Thread to resolve, as `review comment` and `review list` print it.
        thread: u64,
        /// Who resolved it. Defaults to $ROSEN_AGENT, then the OS user name.
        #[arg(long, value_name = "AGENT")]
        by: Option<String>,
    },
    /// List a change's review threads and their comments, oldest first.
    List {
        /// Change whose threads to list: its ID or a unique prefix.
        change: String,
        /// List approval history instead of threads.
        #[arg(long, conflicts_with = "state")]
        approvals: bool,
        /// Only threads in this state: open or resolved.
        #[arg(long)]
        state: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReceiptCommand {
    /// Record that a check set passed on a change's merge result onto one
    /// base commit.
    ///
    /// Checkers run the checks outside Rosen, on the merge result the server
    /// computed for the change onto base commit BASE, then submit a receipt
    /// bound to (HEAD, BASE, check set). A land needs one for every change
    /// that is not prose-only. The server refuses a receipt when HEAD is not
    /// the change's current head, the check set is not the pre-land one, or
    /// the base ref never held BASE. If the base moves on before the land,
    /// the land decides whether the receipt still counts or a new one is
    /// needed. Submitting the same receipt again changes nothing.
    Submit {
        /// Change the checks ran for: its ID or a unique prefix.
        change: String,
        /// Change head the checks ran on, as `rosen change show` prints it.
        #[arg(long, value_name = "HEAD")]
        head: String,
        /// Base commit the change was merged onto for the checks.
        #[arg(long, value_name = "BASE")]
        base: String,
        /// Check set that passed. Only the pre-land check set (build and
        /// lint) is accepted. Defaults to the pre-land check set the base's
        /// project configuration names; `rosen config show` lists it.
        #[arg(long, value_name = "NAME")]
        check_set: Option<String>,
        /// Agent that ran the checks, recorded on the receipt and in the
        /// landed commit. Defaults to $ROSEN_AGENT, then the OS user name.
        #[arg(long, value_name = "AGENT")]
        submitter: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// List workspaces and the change each belongs to.
    List,
    /// Snapshot a local workspace on every write until Ctrl-C.
    ///
    /// Each write is on the server within 2 s. The first snapshot covers
    /// the whole workspace, catching writes made while nothing watched it.
    /// Writes to ignored paths are skipped and never delay a snapshot. A
    /// failed snapshot is retried every second, and its files are kept.
    Watch {
        /// Workspace to watch. Defaults to the one holding the current
        /// directory.
        #[arg(value_name = "DIR")]
        workspace: Option<PathBuf>,
    },
    /// Delete a workspace. Refused, naming the files, while any edit in it
    /// is not yet stored on the server.
    Delete {
        /// Workspace to delete.
        workspace: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show the configuration in force: generated files, registration
    /// lists, sensitive paths, the pre-land and post-land check sets with
    /// their commands, the warm pool size and the mirror remote.
    ///
    /// The server reads it from the base, never from a change, so editing
    /// `rosen.toml` takes effect only once the edit lands. An invalid file
    /// never takes effect: the last valid one stays in force, and this
    /// command prints why the file was refused. Checkers run the pre-land
    /// check set's commands on a change's merge result before submitting a
    /// receipt.
    Show {
        /// Read the configuration at this commit instead of the base.
        #[arg(long, value_name = "COMMIT")]
        at: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// Start this machine's server in the background if it is not already
    /// running, then show its status. Safe to run when it is already up.
    Start,
    /// Show whether the server is running and which project it holds. Exits
    /// with status 1, without starting a server, when none is running.
    Status,
}

/// Paths an operation applies to, for one change.
#[derive(Debug, Args)]
pub struct ClaimArgs {
    /// Change the claim belongs to: its ID or a unique prefix. Defaults to
    /// $ROSEN_CHANGE.
    #[arg(long)]
    pub change: Option<String>,
    /// Repository-relative paths or globs: `?` and `*` match within one
    /// directory, a `**` segment any number of directories, and a trailing
    /// `/` everything under a directory.
    #[arg(required = true)]
    pub paths: Vec<String>,
}

/// What `rosen overlaps` queries: a change, a set of paths, or a change
/// narrowed to some paths.
#[derive(Debug, Args)]
pub struct OverlapArgs {
    /// Change to query: its ID or a unique prefix. Defaults, when no paths
    /// are given, to the workspace holding the current directory, then
    /// $ROSEN_CHANGE.
    #[arg(long)]
    pub change: Option<String>,
    /// Repository-relative paths or globs. Alone, they are queried as if a
    /// change claimed them. With `--change`, only overlaps on these paths
    /// are listed.
    pub paths: Vec<String>,
}

/// What `rosen release` drops: some paths, or every record with `--all`.
#[derive(Debug, Args)]
pub struct ReleaseArgs {
    /// Change the records belong to: its ID or a unique prefix. Defaults to
    /// $ROSEN_CHANGE.
    #[arg(long)]
    pub change: Option<String>,
    /// Release every claim, share and wait the change holds.
    #[arg(long, conflicts_with = "paths")]
    pub all: bool,
    /// Paths or globs to release, exactly as they were claimed.
    #[arg(required_unless_present = "all")]
    pub paths: Vec<String>,
}

/// Which change `rosen status` reports on.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Change to show: its ID or a unique prefix. Defaults to the change of
    /// the workspace holding the current directory, then $ROSEN_CHANGE.
    /// Given alone, no workspace is scanned.
    #[arg(long)]
    pub change: Option<String>,
    /// Workspace to scan. Defaults to the one holding the current directory.
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,
}

/// A change and optional paths that narrow a query.
#[derive(Debug, Args)]
pub struct ChangePaths {
    /// Change to query; defaults to the current workspace's.
    #[arg(long)]
    pub change: Option<String>,
    /// Limit the query to these paths.
    pub paths: Vec<PathBuf>,
}

impl Command {
    /// The command path as typed, for example `change new`, used to name the
    /// client operation and in error messages.
    pub fn path(&self) -> &'static str {
        match self {
            Command::Import { .. } => "import",
            Command::Change(sub) => sub.path(),
            Command::Commit { .. } => "commit",
            Command::Status(_) => "status",
            Command::Diff(_) => "diff",
            Command::Log { .. } => "log",
            Command::Claim(_) => "claim",
            Command::Share(_) => "share",
            Command::Wait(_) => "wait",
            Command::Release(_) => "release",
            Command::Overlaps(_) => "overlaps",
            Command::Review(sub) => sub.path(),
            Command::Receipt(ReceiptCommand::Submit { .. }) => "receipt submit",
            Command::Land { .. } => "land",
            Command::Resolve { .. } => "resolve",
            Command::Workspace(WorkspaceCommand::List) => "workspace list",
            Command::Workspace(WorkspaceCommand::Watch { .. }) => "workspace watch",
            Command::Workspace(WorkspaceCommand::Delete { .. }) => "workspace delete",
            Command::Server(ServerCommand::Start) => "server start",
            Command::Server(ServerCommand::Status) => "server status",
            Command::Config(ConfigCommand::Show { .. }) => "config show",
            Command::Events { .. } => "events",
            Command::Machine(sub) => sub.path(),
        }
    }
}

impl MachineCommand {
    fn path(&self) -> &'static str {
        match self {
            MachineCommand::Join { .. } => "machine join",
            MachineCommand::List => "machine list",
            MachineCommand::Approve { .. } => "machine approve",
            MachineCommand::Revoke { .. } => "machine revoke",
            MachineCommand::Key(MachineKeyCommand::Create { .. }) => "machine key create",
            MachineCommand::Key(MachineKeyCommand::List) => "machine key list",
            MachineCommand::Key(MachineKeyCommand::Revoke { .. }) => "machine key revoke",
        }
    }
}

impl ChangeCommand {
    fn path(&self) -> &'static str {
        match self {
            ChangeCommand::New { .. } => "change new",
            ChangeCommand::List { .. } => "change list",
            ChangeCommand::Show { .. } => "change show",
            ChangeCommand::Abandon { .. } => "change abandon",
        }
    }
}

impl ReviewCommand {
    fn path(&self) -> &'static str {
        match self {
            ReviewCommand::Comment { .. } => "review comment",
            ReviewCommand::Approve { .. } => "review approve",
            ReviewCommand::Resolve { .. } => "review resolve",
            ReviewCommand::List { .. } => "review list",
        }
    }
}
