//! The `rosen` command tree (SPEC.md R10).
//!
//! Help text is written for agents: SPEC.md requires that `--help` alone is
//! enough to learn Rosen, so every command and argument says what it does
//! and when to use it.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
#[command(name = "rosen", version, propagate_version = true)]
pub struct Cli {
    /// Print machine-readable JSON with a versioned schema instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Server pipe name (Windows) or socket path. Defaults to $ROSEN_ENDPOINT,
    /// then the per-user default, so normally you never set it.
    #[arg(long, global = true, value_name = "ENDPOINT")]
    pub endpoint: Option<String>,

    #[command(subcommand)]
    pub command: Command,
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
    /// Snapshots happen automatically within 2 s of each write; commit only
    /// to give a point in the change's history a message.
    Commit {
        /// Message describing the snapshot.
        #[arg(short, long)]
        message: String,
    },
    /// Show the current change, its state, conflicts and unsnapshotted files.
    Status,
    /// Show a change's diff against its parent.
    Diff(ChangePaths),
    /// Show history of the base, a change, or a path.
    Log {
        /// Change to show; defaults to the base.
        #[arg(long)]
        change: Option<String>,
        /// Limit history to these paths.
        paths: Vec<PathBuf>,
        /// Show at most this many entries.
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
    /// waiting on them.
    Release(ClaimArgs),
    /// List changes that overlap a change or a set of paths.
    ///
    /// Two changes overlap when they claim the same path, or their edits in
    /// one file touch or sit within 3 lines of each other.
    Overlaps(ChangePaths),
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
    /// Stream the events that concern you: snapshot, landed, rebased,
    /// conflict, updated, review, claim, released, green, reverted and mirror.
    ///
    /// You get an event when it is about a change you own, review, or stack
    /// on, or one whose claims overlap yours; `mirror` events go to everyone.
    /// Each event carries a sequence number. The server stores how far you
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
        /// Only threads in this state: open or resolved.
        #[arg(long)]
        state: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ReceiptCommand {
    /// Record that a check set passed on a change's merge result onto one
    /// base commit.
    Submit {
        /// Change the checks ran for.
        change: String,
        /// Change head the checks ran on.
        #[arg(long)]
        head: String,
        /// Base commit the change was merged onto.
        #[arg(long)]
        base: String,
        /// Check set that passed, for example `pre-land`.
        #[arg(long, default_value = "pre-land")]
        check_set: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// List workspaces and the change each belongs to.
    List,
    /// Delete a workspace. Refused, naming the files, while any edit in it
    /// is not yet stored on the server.
    Delete {
        /// Workspace to delete.
        workspace: String,
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
            Command::Status => "status",
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
            Command::Workspace(WorkspaceCommand::Delete { .. }) => "workspace delete",
            Command::Server(ServerCommand::Start) => "server start",
            Command::Server(ServerCommand::Status) => "server status",
            Command::Events { .. } => "events",
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
