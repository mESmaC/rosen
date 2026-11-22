//! Secret-free default labels for `rosen run` timing telemetry (SPEC R9, R10, #463).
//!
//! Only the normalized executable name and a recognized subcommand from a fixed
//! allowlist are kept. Arbitrary positional arguments, script paths, flags and
//! environment values never enter persisted timing labels.

/// Maximum wrapper layers (`sh -c`, `cmd /C`, `env`, `nice`) unwrapped.
const MAX_UNWRAP_DEPTH: usize = 3;

struct ToolSpec {
    program: &'static str,
    value_flags: &'static [&'static str],
    subcommands: &'static [&'static str],
}

/// Tools whose first positional argument names an operation rather than data.
const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        program: "cargo",
        value_flags: &[
            "-C",
            "-Z",
            "--color",
            "--config",
            "--manifest-path",
            "--target-dir",
        ],
        subcommands: &[
            "add",
            "audit",
            "bench",
            "build",
            "check",
            "clean",
            "clippy",
            "deny",
            "doc",
            "expand",
            "fetch",
            "fix",
            "fmt",
            "generate-lockfile",
            "install",
            "llvm-cov",
            "locate-project",
            "login",
            "logout",
            "metadata",
            "miri",
            "new",
            "nextest",
            "outdated",
            "owner",
            "package",
            "pkgid",
            "publish",
            "read-manifest",
            "remove",
            "report",
            "run",
            "rustc",
            "rustdoc",
            "search",
            "tarpaulin",
            "test",
            "tree",
            "udeps",
            "uninstall",
            "update",
            "vendor",
            "verify-project",
            "version",
            "yank",
        ],
    },
    ToolSpec {
        program: "lake",
        value_flags: &["-d", "--dir", "-f", "--file", "--packages", "--lean"],
        subcommands: &[
            "build",
            "check-lint",
            "check-test",
            "clean",
            "env",
            "exe",
            "init",
            "lint",
            "new",
            "pack",
            "print-paths",
            "query",
            "resolve-deps",
            "script",
            "self-check",
            "serve",
            "setup-file",
            "test",
            "translate-config",
            "unpack",
            "update",
            "upload",
        ],
    },
    ToolSpec {
        program: "pnpm",
        value_flags: &[
            "-C",
            "--dir",
            "-F",
            "--filter",
            "--loglevel",
            "--reporter",
            "--workspace-concurrency",
        ],
        subcommands: JS_PM_SUBCOMMANDS,
    },
    ToolSpec {
        program: "npm",
        value_flags: &["-C", "--prefix", "--loglevel", "--workspace", "-w"],
        subcommands: JS_PM_SUBCOMMANDS,
    },
    ToolSpec {
        program: "yarn",
        value_flags: &["--cwd"],
        subcommands: JS_PM_SUBCOMMANDS,
    },
    ToolSpec {
        program: "bun",
        value_flags: &["--cwd", "-c", "--config"],
        subcommands: JS_PM_SUBCOMMANDS,
    },
    ToolSpec {
        program: "git",
        value_flags: &[
            "-C",
            "-c",
            "--config-env",
            "--exec-path",
            "--git-dir",
            "--namespace",
            "--super-prefix",
            "--work-tree",
        ],
        subcommands: &[
            "add",
            "am",
            "apply",
            "archive",
            "bisect",
            "blame",
            "branch",
            "bundle",
            "cat-file",
            "check-ignore",
            "checkout",
            "cherry",
            "cherry-pick",
            "clean",
            "clone",
            "commit",
            "config",
            "describe",
            "diff",
            "diff-files",
            "diff-index",
            "diff-tree",
            "fetch",
            "format-patch",
            "fsck",
            "gc",
            "grep",
            "hash-object",
            "init",
            "log",
            "ls-files",
            "ls-remote",
            "ls-tree",
            "merge",
            "merge-base",
            "merge-tree",
            "mv",
            "notes",
            "pack-objects",
            "pack-refs",
            "prune",
            "pull",
            "push",
            "range-diff",
            "read-tree",
            "rebase",
            "reflog",
            "remote",
            "repack",
            "replace",
            "reset",
            "restore",
            "rev-list",
            "rev-parse",
            "revert",
            "rm",
            "shortlog",
            "show",
            "show-ref",
            "sparse-checkout",
            "stash",
            "status",
            "submodule",
            "switch",
            "symbolic-ref",
            "tag",
            "update-index",
            "update-ref",
            "verify-commit",
            "verify-pack",
            "verify-tag",
            "whatchanged",
            "worktree",
            "write-tree",
        ],
    },
    ToolSpec {
        program: "rosen",
        value_flags: &["--endpoint", "--data-dir"],
        subcommands: &[
            "change",
            "checker",
            "claim",
            "collapse",
            "commit",
            "config",
            "diff",
            "events",
            "import",
            "land",
            "log",
            "machine",
            "mcp",
            "mirror",
            "overlaps",
            "rebase",
            "receipt",
            "release",
            "resolve",
            "review",
            "run",
            "server",
            "share",
            "stats",
            "status",
            "wait",
            "workspace",
        ],
    },
    ToolSpec {
        program: "rosen-faults",
        value_flags: &[],
        subcommands: &["drill"],
    },
    ToolSpec {
        program: "rosen-stress",
        value_flags: &[],
        subcommands: &["agents", "checker", "local", "seed", "verify"],
    },
    ToolSpec {
        program: "rustup",
        value_flags: &[],
        subcommands: &[
            "check",
            "component",
            "default",
            "doc",
            "override",
            "run",
            "self",
            "set",
            "show",
            "target",
            "toolchain",
            "update",
            "which",
        ],
    },
    ToolSpec {
        program: "go",
        value_flags: &["-C"],
        subcommands: &[
            "bug",
            "build",
            "clean",
            "doc",
            "env",
            "fix",
            "fmt",
            "generate",
            "get",
            "install",
            "list",
            "mod",
            "run",
            "telemetry",
            "test",
            "tool",
            "version",
            "vet",
            "work",
        ],
    },
    ToolSpec {
        program: "dotnet",
        value_flags: &[],
        subcommands: &[
            "add",
            "build",
            "build-server",
            "clean",
            "format",
            "fsi",
            "help",
            "list",
            "msbuild",
            "new",
            "nuget",
            "pack",
            "publish",
            "remove",
            "restore",
            "run",
            "sdk",
            "sln",
            "store",
            "test",
            "tool",
            "vstest",
            "workload",
        ],
    },
    ToolSpec {
        program: "uv",
        value_flags: &["--cache-dir", "--config-file", "--directory", "--project"],
        subcommands: &[
            "add", "auth", "build", "cache", "export", "init", "lock", "pip", "publish", "python",
            "remove", "run", "self", "sync", "tool", "tree", "venv", "version",
        ],
    },
    ToolSpec {
        program: "pip",
        value_flags: &[],
        subcommands: PIP_SUBCOMMANDS,
    },
    ToolSpec {
        program: "pip3",
        value_flags: &[],
        subcommands: PIP_SUBCOMMANDS,
    },
    ToolSpec {
        program: "ruff",
        value_flags: &[],
        subcommands: &[
            "analyze", "check", "clean", "config", "format", "linter", "rule", "server", "version",
        ],
    },
    ToolSpec {
        program: "gh",
        value_flags: &[],
        subcommands: &[
            "auth",
            "browse",
            "cache",
            "codespace",
            "completion",
            "config",
            "extension",
            "gist",
            "gpg-key",
            "issue",
            "label",
            "org",
            "pr",
            "project",
            "release",
            "repo",
            "ruleset",
            "run",
            "search",
            "secret",
            "ssh-key",
            "status",
            "variable",
            "workflow",
        ],
    },
    ToolSpec {
        program: "docker",
        value_flags: &["-H", "--host", "-c", "--context", "--config"],
        subcommands: CONTAINER_SUBCOMMANDS,
    },
    ToolSpec {
        program: "podman",
        value_flags: &["-c", "--connection", "--root", "--runroot"],
        subcommands: CONTAINER_SUBCOMMANDS,
    },
    ToolSpec {
        program: "zig",
        value_flags: &[],
        subcommands: &[
            "ast-check",
            "build",
            "build-exe",
            "build-lib",
            "build-obj",
            "c++",
            "cc",
            "fetch",
            "fmt",
            "init",
            "run",
            "test",
            "translate-c",
            "version",
        ],
    },
    ToolSpec {
        program: "swift",
        value_flags: &[],
        subcommands: &["build", "package", "run", "sdk", "test"],
    },
    ToolSpec {
        program: "bazel",
        value_flags: &[],
        subcommands: &[
            "aquery", "build", "clean", "coverage", "cquery", "dump", "fetch", "help", "info",
            "mod", "query", "run", "shutdown", "sync", "test", "vendor", "version",
        ],
    },
    ToolSpec {
        program: "nix",
        value_flags: &[],
        subcommands: &[
            "build", "bundle", "copy", "develop", "edit", "eval", "flake", "fmt", "hash", "log",
            "nar", "profile", "registry", "repl", "run", "search", "shell", "store",
        ],
    },
];

const JS_PM_SUBCOMMANDS: &[&str] = &[
    "add",
    "audit",
    "build",
    "check",
    "ci",
    "clean",
    "create",
    "dedupe",
    "dev",
    "dlx",
    "doctor",
    "env",
    "exec",
    "fetch",
    "fmt",
    "format",
    "info",
    "init",
    "install",
    "licenses",
    "link",
    "lint",
    "list",
    "ls",
    "outdated",
    "pack",
    "patch",
    "prune",
    "publish",
    "rebuild",
    "remove",
    "rm",
    "run",
    "serve",
    "start",
    "store",
    "test",
    "type-check",
    "typecheck",
    "uninstall",
    "unlink",
    "update",
    "upgrade",
    "version",
    "view",
    "why",
    "x",
];

const PIP_SUBCOMMANDS: &[&str] = &[
    "cache",
    "check",
    "config",
    "debug",
    "download",
    "freeze",
    "hash",
    "help",
    "index",
    "inspect",
    "install",
    "list",
    "lock",
    "search",
    "show",
    "uninstall",
    "wheel",
];

const CONTAINER_SUBCOMMANDS: &[&str] = &[
    "attach",
    "build",
    "buildx",
    "commit",
    "compose",
    "container",
    "context",
    "cp",
    "create",
    "diff",
    "events",
    "exec",
    "export",
    "history",
    "image",
    "images",
    "import",
    "info",
    "inspect",
    "kill",
    "load",
    "login",
    "logout",
    "logs",
    "manifest",
    "network",
    "node",
    "pause",
    "plugin",
    "port",
    "ps",
    "pull",
    "push",
    "rename",
    "restart",
    "rm",
    "rmi",
    "run",
    "save",
    "search",
    "secret",
    "service",
    "stack",
    "start",
    "stats",
    "stop",
    "swarm",
    "system",
    "tag",
    "top",
    "trust",
    "unpause",
    "update",
    "version",
    "volume",
    "wait",
];

/// Derives a stable, secret-free command label (e.g. `cargo test`, `lake build`, `echo`).
pub(super) fn command_label(command: &[String]) -> String {
    let mut tokens: Vec<String> = command.to_vec();
    let mut fallback: Option<String> = None;

    for _ in 0..MAX_UNWRAP_DEPTH {
        let Some(first) = tokens.first() else {
            break;
        };
        let Some(program) = program_name(first) else {
            break;
        };
        fallback.get_or_insert_with(|| program.clone());
        match unwrap_wrapper(&program, &tokens[1..]) {
            Some(inner) if !inner.is_empty() => tokens = inner,
            Some(_) => return fallback.unwrap_or_else(|| "run".to_owned()),
            None => return format_label(&program, &tokens[1..]),
        }
    }

    if let Some(first) = tokens.first() {
        if let Some(program) = program_name(first) {
            return format_label(&program, &tokens[1..]);
        }
    }
    fallback.unwrap_or_else(|| "run".to_owned())
}

fn format_label(program: &str, args: &[String]) -> String {
    match recognized_subcommand(program, args) {
        Some(sub) => format!("{program} {sub}"),
        None => program.to_owned(),
    }
}

fn recognized_subcommand(program: &str, args: &[String]) -> Option<&'static str> {
    let spec = TOOLS.iter().find(|tool| tool.program == program)?;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return None;
        }
        if program == "cargo" && arg.starts_with('+') {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            if !arg.contains('=') && spec.value_flags.contains(&arg) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Only the first positional argument can be a subcommand; stop if unrecognized.
        return spec
            .subcommands
            .iter()
            .copied()
            .find(|sub| sub.eq_ignore_ascii_case(arg));
    }
    None
}

/// Unwraps one layer of a shell or command-prefix wrapper (`cmd`, `sh`, `pwsh`, `env`, `nice`).
/// Returns `None` when `program` is not a wrapper or runs a script file directly.
fn unwrap_wrapper(program: &str, args: &[String]) -> Option<Vec<String>> {
    match program {
        "cmd" => unwrap_cmd(args),
        "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh" => unwrap_posix_shell(args),
        "pwsh" | "powershell" => unwrap_powershell(args),
        "env" => unwrap_env(args),
        "nice" => unwrap_nice(args),
        _ => None,
    }
}

fn unwrap_cmd(args: &[String]) -> Option<Vec<String>> {
    for (idx, arg) in args.iter().enumerate() {
        if arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k") {
            let rest = &args[idx + 1..];
            return Some(match rest {
                [] => Vec::new(),
                [single] => pick_shell_command(single),
                many => many.to_vec(),
            });
        }
        if !arg.starts_with('/') {
            return None;
        }
    }
    None
}

fn unwrap_posix_shell(args: &[String]) -> Option<Vec<String>> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" || !arg.starts_with(['-', '+']) || arg.len() < 2 {
            return None;
        }
        if matches!(arg, "-o" | "+o") {
            i += 2;
            continue;
        }
        if arg.starts_with('-')
            && !arg.starts_with("--")
            && arg[1..].bytes().all(|b| b.is_ascii_alphabetic())
            && arg[1..].contains('c')
        {
            return Some(
                args.get(i + 1)
                    .map_or_else(Vec::new, |s| pick_shell_command(s)),
            );
        }
        i += 1;
    }
    None
}

fn unwrap_powershell(args: &[String]) -> Option<Vec<String>> {
    const VALUE_FLAGS: &[&str] = &[
        "-configurationname",
        "-custompipename",
        "-ep",
        "-executionpolicy",
        "-inputformat",
        "-outputformat",
        "-settingsfile",
        "-wd",
        "-windowstyle",
        "-workingdirectory",
    ];
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg.eq_ignore_ascii_case("-command") || arg.eq_ignore_ascii_case("-c") {
            let rest = &args[i + 1..];
            return Some(match rest {
                [] => Vec::new(),
                [single] => pick_shell_command(single),
                many => many.to_vec(),
            });
        }
        if arg.eq_ignore_ascii_case("-file")
            || arg.eq_ignore_ascii_case("-f")
            || !arg.starts_with('-')
        {
            return None;
        }
        let lower = arg.to_ascii_lowercase();
        if VALUE_FLAGS.contains(&lower.as_str()) {
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

fn unwrap_env(args: &[String]) -> Option<Vec<String>> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return Some(args[i + 1..].to_vec());
        }
        if matches!(arg, "-u" | "--unset" | "-C" | "--chdir") {
            i += 2;
            continue;
        }
        if arg.starts_with('-') || is_env_assignment(arg) {
            i += 1;
            continue;
        }
        return Some(args[i..].to_vec());
    }
    Some(Vec::new())
}

fn unwrap_nice(args: &[String]) -> Option<Vec<String>> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return Some(args[i + 1..].to_vec());
        }
        if matches!(arg, "-n" | "--adjustment") {
            i += 2;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(args[i..].to_vec());
    }
    Some(Vec::new())
}

/// Extracts the first runnable command segment from a shell string, skipping
/// leading environment assignments and setup commands (`cd`, `set`, `export`).
fn pick_shell_command(script: &str) -> Vec<String> {
    let mut fallback_setup: Option<String> = None;
    for segment in split_shell_segments(script) {
        let first_cmd = segment.iter().position(|tok| !is_env_assignment(tok));
        let Some(start) = first_cmd else {
            continue;
        };
        let rest = &segment[start..];
        let Some(name) = program_name(&rest[0]) else {
            return Vec::new();
        };
        if is_shell_keyword(&name) {
            return Vec::new();
        }
        if is_setup_builtin(&name) {
            fallback_setup.get_or_insert(name);
            continue;
        }
        return rest.to_vec();
    }
    fallback_setup.into_iter().collect()
}

/// Splits a shell script into command segments on unquoted `&&`, `||`, `;`, `|`,
/// `&` and newlines, stripping matching single/double quotes from tokens.
fn split_shell_segments(script: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut current_seg = Vec::new();
    let mut current_tok = String::new();
    let mut quote: Option<char> = None;
    let mut chars = script.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else if q == '"' && ch == '\\' && matches!(chars.peek(), Some('"' | '\\')) {
                if let Some(escaped) = chars.next() {
                    current_tok.push(escaped);
                }
            } else {
                current_tok.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            c if c == '\n' || c == '\r' || c == ';' || c == '|' || c == '&' => {
                if (c == '|' || c == '&') && chars.peek() == Some(&c) {
                    chars.next();
                }
                flush_token(&mut current_seg, &mut current_tok);
                if !current_seg.is_empty() {
                    segments.push(std::mem::take(&mut current_seg));
                }
            }
            c if c.is_whitespace() => flush_token(&mut current_seg, &mut current_tok),
            _ => current_tok.push(ch),
        }
    }
    flush_token(&mut current_seg, &mut current_tok);
    if !current_seg.is_empty() {
        segments.push(current_seg);
    }
    segments
}

fn flush_token(segment: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        segment.push(std::mem::take(token));
    }
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn is_setup_builtin(name: &str) -> bool {
    matches!(
        name,
        "cd" | "chdir"
            | "export"
            | "popd"
            | "pushd"
            | "set"
            | "set-location"
            | "sl"
            | "umask"
            | "unset"
    )
}

fn is_shell_keyword(name: &str) -> bool {
    matches!(
        name,
        "begin"
            | "case"
            | "catch"
            | "do"
            | "done"
            | "elif"
            | "else"
            | "end"
            | "esac"
            | "fi"
            | "finally"
            | "for"
            | "foreach"
            | "function"
            | "if"
            | "process"
            | "select"
            | "switch"
            | "then"
            | "try"
            | "until"
            | "while"
    )
}

/// Normalizes a command token to its lowercase executable name, rejecting
/// expressions, assignments and shell syntax that cannot be a program name.
fn program_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_matches(|c| c == '"' || c == '\'');
    if trimmed.is_empty() || is_env_assignment(trimmed) {
        return None;
    }
    let base = trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())?;
    let stem = strip_executable_ext(base);
    if stem.is_empty() || stem.contains("..") {
        return None;
    }
    let first = stem.as_bytes()[0];
    if !first.is_ascii_alphanumeric() {
        return None;
    }
    if !stem.bytes().any(|b| b.is_ascii_alphabetic())
        || !stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
    {
        return None;
    }
    let lower = stem.to_ascii_lowercase();
    if lower.starts_with("echo.") || lower.starts_with("echo+") {
        return Some("echo".to_owned());
    }
    Some(lower)
}

fn strip_executable_ext(name: &str) -> &str {
    for ext in [".exe", ".cmd", ".bat", ".com", ".ps1", ".sh"] {
        if name.len() > ext.len() && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext) {
            return &name[..name.len() - ext.len()];
        }
    }
    name
}

#[cfg(test)]
mod tests {
    use super::command_label;

    fn label(args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        command_label(&owned)
    }

    /// Risk (R9, #463): default run timing labels are persisted on the server
    /// and sent in R9 stats; positional arguments must never leak into labels,
    /// while recognized build/check subcommands stay stable across shells.
    #[test]
    fn default_labels_keep_recognized_subcommands_and_drop_positional_args() {
        assert_eq!(label(&["echo", "secret-arg"]), "echo");
        assert_eq!(label(&["cmd", "/C", "echo secret-arg"]), "echo");
        assert_eq!(label(&["sh", "-c", "echo secret-arg"]), "echo");
        assert_eq!(label(&["python", "secret_script.py", "token"]), "python");
        assert_eq!(label(&["cargo", "secret-arg"]), "cargo");
        assert_eq!(label(&["cargo", "secret-arg", "test"]), "cargo");
        assert_eq!(label(&["pnpm", "-C", "ui", "secret-script"]), "pnpm");

        assert_eq!(
            label(&["cargo", "+nightly", "-q", "test", "--workspace"]),
            "cargo test"
        );
        assert_eq!(
            label(&["cmd", "/C", "cargo clippy --workspace -- -D warnings"]),
            "cargo clippy"
        );
        assert_eq!(
            label(&["sh", "-c", "cd proofs && lake build --wfail"]),
            "lake build"
        );
        assert_eq!(
            label(&[
                "sh",
                "-c",
                "cd proofs && lake env lean --run ../.github/scripts/audit_axioms.lean"
            ]),
            "lake env"
        );
        assert_eq!(label(&["pnpm", "-C", "ui", "lint"]), "pnpm lint");
        assert_eq!(label(&["env", "SECRET=1", "cargo", "build"]), "cargo build");
        assert_eq!(label(&["sh", "-c", "SECRET=1"]), "sh");
        assert_eq!(label(&["sh", "-c", "cd /private/dir"]), "cd");
        assert_eq!(
            label(&[
                "powershell.exe",
                "-NoProfile",
                "-Command",
                "1..45 | ForEach-Object { $_ }"
            ]),
            "powershell"
        );
    }
}
