//! Reads a source git repository by running the `git` binary.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use rosen_core::{ObjectId, ObjectKind};

use super::ImportError;

/// Environment variables that would make git read a different repository
/// than the one named, for example when the server was started from a git
/// hook. They are removed from every git command import runs.
const REPOSITORY_OVERRIDES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

/// Receives each object read from git: its id, type and content.
pub type ObjectVisitor<'a> =
    dyn FnMut(ObjectId, ObjectKind, Vec<u8>) -> Result<(), ImportError> + 'a;

/// A git repository opened for reading.
pub struct GitSource {
    repository: PathBuf,
    /// The temporary bare clone made for a URL source, removed on drop.
    temporary_clone: Option<PathBuf>,
}

impl GitSource {
    /// Opens a local repository, or makes a temporary bare clone of a URL
    /// under `scratch_dir`.
    pub fn open(source: &str, scratch_dir: PathBuf) -> Result<Self, ImportError> {
        let local = Path::new(source);
        if local.exists() {
            let repository = std::fs::canonicalize(local)?;
            let opened = Self {
                repository,
                temporary_clone: None,
            };
            opened.git(["rev-parse", "--git-dir"])?;
            return Ok(opened);
        }
        Self::clone_url(source, scratch_dir)
    }

    fn clone_url(url: &str, scratch_dir: PathBuf) -> Result<Self, ImportError> {
        std::fs::create_dir_all(&scratch_dir)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let clone = scratch_dir.join(format!("import-{}-{nanos}.git", std::process::id()));
        // The `--` stops a URL that starts with `-` from being read as an option.
        let mut command = git_command(&scratch_dir);
        command
            .args(["clone", "--bare", "--quiet", "--"])
            .arg(url)
            .arg(&clone);
        run_to_string(command, "git clone")?;
        Ok(Self {
            repository: clone.clone(),
            temporary_clone: Some(clone),
        })
    }

    /// Refuses sources whose object ids the sha1 store cannot keep, and
    /// shallow sources, whose history is missing the commits past the
    /// shallow boundary.
    pub fn require_sha1(&self) -> Result<(), ImportError> {
        let format = self.git(["rev-parse", "--show-object-format"])?;
        if format != "sha1" {
            return Err(ImportError::UnsupportedObjectFormat(format));
        }
        if self.git(["rev-parse", "--is-shallow-repository"])? == "true" {
            return Err(ImportError::Git {
                command: "git rev-parse --is-shallow-repository".into(),
                message: "the source is a shallow clone; import needs its full history".into(),
            });
        }
        Ok(())
    }

    /// The branch the source's `HEAD` names.
    pub fn default_branch(&self) -> Result<String, ImportError> {
        let head = self
            .git(["symbolic-ref", "--quiet", "HEAD"])
            .map_err(|_| ImportError::NoDefaultBranch)?;
        head.strip_prefix("refs/heads/")
            .map(str::to_owned)
            .ok_or(ImportError::NoDefaultBranch)
    }

    /// The commit at the tip of `branch`.
    pub fn branch_tip(&self, branch: &str) -> Result<ObjectId, ImportError> {
        let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        // A full ref name keeps a branch called like an option or a commit
        // id from being read as one.
        let spec = format!("refs/heads/{short}^{{commit}}");
        let tip = self
            .git(["rev-parse", "--verify", "--quiet", spec.as_str()])
            .map_err(|_| ImportError::BranchNotFound(short.to_owned()))?;
        parse_id(&tip)
    }

    /// Ids of every object reachable from `commit`: its commits, trees and
    /// blobs.
    pub fn reachable_objects(&self, commit: &ObjectId) -> Result<Vec<ObjectId>, ImportError> {
        let commit = commit.to_hex();
        let listing = self.git([
            "rev-list",
            "--objects",
            "--no-object-names",
            commit.as_str(),
        ])?;
        listing.lines().map(parse_id).collect()
    }

    /// Streams the objects named by `ids` to `each`, in order, through one
    /// `git cat-file --batch` process. Stops at the first error `each`
    /// returns.
    pub fn read_objects(
        &self,
        ids: &[ObjectId],
        each: &mut ObjectVisitor<'_>,
    ) -> Result<(), ImportError> {
        let mut command = self.command();
        command.args(["cat-file", "--batch"]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| spawn_error("git cat-file", e))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        std::thread::scope(|scope| {
            // Requests and replies flow at once: git blocks writing replies
            // nobody reads, so a separate thread feeds the requests.
            let feeder = scope.spawn(move || feed_ids(stdin, ids));
            let errors = scope.spawn(move || read_all(stderr));
            let read = read_batch(BufReader::new(stdout), ids.len(), each);
            finish_cat_file(&mut child, read, feeder.join(), errors.join())
        })
    }

    fn command(&self) -> Command {
        git_command(&self.repository)
    }

    /// Runs git in the source and returns its trimmed standard output.
    fn git<const N: usize>(&self, args: [&str; N]) -> Result<String, ImportError> {
        let mut command = self.command();
        command.args(args);
        run_to_string(command, &format!("git {}", args.join(" ")))
    }
}

impl Drop for GitSource {
    fn drop(&mut self) {
        if let Some(clone) = &self.temporary_clone {
            // Best effort: a leftover clone in the scratch directory wastes
            // space but loses nothing.
            let _ = std::fs::remove_dir_all(clone);
        }
    }
}

pub(crate) fn git_command(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(directory);
    for name in REPOSITORY_OVERRIDES {
        command.env_remove(name);
    }
    // Stops git from searching the parent directories when `directory` is
    // not itself a repository, so import never reads an enclosing one.
    if let Some(parent) = directory.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command.stdin(Stdio::null());
    command
}

fn run_to_string(mut command: Command, label: &str) -> Result<String, ImportError> {
    let output = command.output().map_err(|e| spawn_error(label, e))?;
    if !output.status.success() {
        return Err(ImportError::Git {
            command: label.to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| ImportError::Protocol(format!("`{label}` printed non-UTF-8 output")))?;
    Ok(stdout.trim().to_owned())
}

fn spawn_error(label: &str, error: std::io::Error) -> ImportError {
    ImportError::Git {
        command: label.to_owned(),
        message: format!("could not run git: {error}"),
    }
}

fn feed_ids(mut stdin: impl Write, ids: &[ObjectId]) -> std::io::Result<()> {
    let mut buffered = std::io::BufWriter::new(&mut stdin);
    for id in ids {
        buffered.write_all(id.to_hex().as_bytes())?;
        buffered.write_all(b"\n")?;
    }
    buffered.flush()
}

fn read_all(mut stream: impl Read) -> String {
    let mut text = String::new();
    let _ = stream.read_to_string(&mut text);
    text
}

/// Parses `count` replies of `git cat-file --batch`: a header line
/// `<id> <type> <size>`, then `size` bytes of content and a newline.
fn read_batch(
    mut replies: impl BufRead,
    count: usize,
    each: &mut ObjectVisitor<'_>,
) -> Result<(), ImportError> {
    let mut header = String::new();
    for _ in 0..count {
        header.clear();
        replies.read_line(&mut header)?;
        let (id, kind, size) = parse_header(header.trim_end())?;
        let mut content = vec![0; size];
        replies.read_exact(&mut content)?;
        let mut newline = [0; 1];
        replies.read_exact(&mut newline)?;
        each(id, kind, content)?;
    }
    Ok(())
}

fn parse_header(header: &str) -> Result<(ObjectId, ObjectKind, usize), ImportError> {
    if let Some(id) = header.strip_suffix(" missing") {
        return Err(ImportError::Protocol(format!(
            "object {id} is missing from the source"
        )));
    }
    let mut fields = header.split(' ');
    let (Some(id), Some(kind), Some(size), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(ImportError::Protocol(format!(
            "bad cat-file header `{header}`"
        )));
    };
    let kind = ObjectKind::parse(kind.as_bytes())
        .map_err(|_| ImportError::Protocol(format!("unexpected object type `{kind}`")))?;
    let size = size
        .parse()
        .map_err(|_| ImportError::Protocol(format!("bad object size in `{header}`")))?;
    Ok((parse_id(id)?, kind, size))
}

fn parse_id(hex: &str) -> Result<ObjectId, ImportError> {
    ObjectId::from_hex(hex).map_err(|_| ImportError::Protocol(format!("bad object id `{hex}`")))
}

/// Ends the cat-file process and picks the most useful error to report.
fn finish_cat_file(
    child: &mut Child,
    read: Result<(), ImportError>,
    feeder: std::thread::Result<std::io::Result<()>>,
    stderr: std::thread::Result<String>,
) -> Result<(), ImportError> {
    if read.is_err() {
        // Killing git also unblocks the feeder thread's pending write.
        let _ = child.kill();
    }
    let status = child.wait()?;
    let git_failed = |message: String| ImportError::Git {
        command: "git cat-file --batch".into(),
        message,
    };
    if !status.success() && read.is_ok() {
        return Err(git_failed(stderr.unwrap_or_default().trim().to_owned()));
    }
    read?;
    match feeder {
        Ok(result) => result.map_err(ImportError::Io),
        Err(_) => Err(git_failed("the request writer thread panicked".into())),
    }
}
