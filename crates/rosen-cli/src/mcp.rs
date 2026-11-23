//! `rosen mcp`: a Model Context Protocol server on stdin and stdout (R10).
//!
//! It offers the `rosen` commands an agent works with as tools, so an agent
//! calls them with named arguments instead of composing shell lines. Nothing
//! is built twice: the tools are read from the command definitions
//! (`mcp_tools`), and a call runs this same program with `--json`, returning
//! the versioned JSON object as `structuredContent` alongside a text copy.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::commands::McpArgs;
use crate::mcp_tools::{self, Tool};
use crate::output::{ERROR_SCHEMA, RUN_SCHEMA};

/// The protocol revision answered when the client names none.
/// `2025-06-18` is the revision that defines `structuredContent` on tool calls.
const PROTOCOL_VERSION: &str = "2025-06-18";

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const PARSE_ERROR: i64 = -32700;

const INSTRUCTIONS: &str = "Rosen is this project's version control. Each tool runs the `rosen` \
command of the same name and returns its versioned JSON as `structuredContent` (plus a text copy); \
a refusal comes back as an error with Rosen's reason. Every file written in a Rosen workspace is \
stored on the server within 2 seconds, so there is no add, push or pull. Tools that take an \
optional `change` act on this workspace's change when it is left out.";

const MANAGED_INSTRUCTIONS: &str = "Rosen is this project's version control, in a host-managed \
workspace ($ROSEN_CHANGE). Each tool runs the `rosen` command of the same name and returns its \
versioned JSON as `structuredContent` (plus a text copy); a refusal comes back as an error with \
Rosen's reason. Your host owns the watcher, checks, review, landing and workspace deletion, so \
those commands are hidden. Call `commit` to name your work when checks pass, `wait` to park on \
claimed paths, `events` to read pending events, and `run` to run builds or tests in the run queue.";

/// A request's failure: a JSON-RPC error code and what went wrong.
type Refusal = (i64, String);

struct Server {
    tools: Vec<Tool>,
    managed: bool,
    /// This program, run again for each call.
    program: PathBuf,
    endpoint: Option<String>,
    workspace: Option<PathBuf>,
}

/// What a child `rosen` invocation produced.
struct ToolOutput {
    text: String,
    structured: Option<Value>,
    failed: bool,
}

/// Serves requests, one JSON object per line, until stdin closes.
pub fn serve(args: &McpArgs, endpoint: Option<&str>) -> std::io::Result<()> {
    let managed = crate::managed::is_managed();
    let server = Server {
        tools: mcp_tools::tools(args.landing, managed),
        managed,
        program: std::env::current_exe()?,
        endpoint: endpoint.map(str::to_owned),
        workspace: args.workspace.clone(),
    };
    let mut output = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.answer(&line) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

impl Server {
    /// The reply to one message, or `None` when it takes none: a
    /// notification, or the client's reply to something.
    fn answer(&self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => return Some(refusal(Value::Null, PARSE_ERROR, &error.to_string())),
        };
        let id = message.get("id").cloned()?;
        let method = message.get("method").and_then(Value::as_str)?;
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(initialized(&params, self.managed)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": self.tools.iter().map(Tool::listing).collect::<Vec<Value>>(),
            })),
            "tools/call" => self.call(&params),
            _ => Err((METHOD_NOT_FOUND, format!("no method `{method}`"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, why)) => refusal(id, code, &why),
        })
    }

    /// Runs a tool. A command that fails or is refused is a result marked as
    /// an error, which the agent reads; only a call that names no tool is a
    /// protocol error.
    fn call(&self, params: &Value) -> Result<Value, Refusal> {
        let name = params.get("name").and_then(Value::as_str);
        let tool = name
            .and_then(|name| self.tools.iter().find(|tool| tool.name == name))
            .ok_or_else(|| {
                let name = name.unwrap_or_default();
                (INVALID_PARAMS, format!("no tool `{name}`"))
            })?;
        let arguments = params.get("arguments").unwrap_or(&Value::Null);
        let command = tool.command_path();
        let outcome = match tool.command_line(arguments) {
            Ok(words) if tool.is_run() => self.run_queued(&command, &words),
            Ok(words) => self.run_json(&command, &words),
            Err(why) => error_output(&command, &why),
        };
        let mut reply = json!({
            "content": [{ "type": "text", "text": outcome.text }],
            "isError": outcome.failed,
        });
        if let Some(structured) = outcome.structured {
            reply["structuredContent"] = structured;
        }
        Ok(reply)
    }

    /// Runs `rosen --json <words>` and parses its versioned JSON line(s) into
    /// `structuredContent`.
    fn run_json(&self, command_path: &str, words: &[String]) -> ToolOutput {
        let mut command = self.child_command(true, words);
        let output = match command.output() {
            Ok(output) => output,
            Err(error) => return spawn_failed(command_path, &self.program, &error),
        };
        let printed = String::from_utf8_lossy(&output.stdout);
        let errors = String::from_utf8_lossy(&output.stderr);
        let structured = parse_structured(&printed);
        let text = join_streams(&printed, &errors);
        let failed = !output.status.success();
        if failed && structured.is_none() {
            let reason = if text.is_empty() {
                format!("command exited with {}", output.status)
            } else {
                text
            };
            return error_output(command_path, &reason);
        }
        ToolOutput {
            text,
            structured,
            failed,
        }
    }

    /// Runs `rosen run ...` without `--json` so `run` keeps its log file and
    /// diagnostic summary (R3), then wraps the result in `rosen.run/v1`.
    fn run_queued(&self, command_path: &str, words: &[String]) -> ToolOutput {
        let mut command = self.child_command(false, words);
        let output = match command.output() {
            Ok(output) => output,
            Err(error) => return spawn_failed(command_path, &self.program, &error),
        };
        let printed = String::from_utf8_lossy(&output.stdout);
        let errors = String::from_utf8_lossy(&output.stderr);
        let combined = join_streams(&printed, &errors);
        let log = printed
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("Log: "))
            .map(str::to_owned);
        let structured = json!({
            "schema": RUN_SCHEMA,
            "exit_code": output.status.code(),
            "success": output.status.success(),
            "output": combined,
            "log": log,
        });
        ToolOutput {
            text: structured.to_string(),
            structured: Some(structured),
            failed: !output.status.success(),
        }
    }

    fn child_command(&self, json_flag: bool, words: &[String]) -> Command {
        let mut command = Command::new(&self.program);
        if json_flag {
            command.arg("--json");
        }
        // Before the words: they can end with `--` and positional values.
        if let Some(endpoint) = &self.endpoint {
            command.arg(format!("--endpoint={endpoint}"));
        }
        command.args(words).stdin(Stdio::null());
        if let Some(workspace) = &self.workspace {
            command.current_dir(workspace);
        }
        without_console_window(&mut command);
        command
    }
}

fn spawn_failed(command: &str, program: &std::path::Path, error: &std::io::Error) -> ToolOutput {
    let reason = format!("could not run {}: {error}", program.display());
    error_output(command, &reason)
}

fn error_output(command: &str, error: &str) -> ToolOutput {
    let structured = json!({
        "schema": ERROR_SCHEMA,
        "command": command,
        "error": error,
    });
    ToolOutput {
        text: structured.to_string(),
        structured: Some(structured),
        failed: true,
    }
}

fn join_streams(stdout: &str, stderr: &str) -> String {
    [stdout.trim_end(), stderr.trim_end()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses one or more JSON object lines printed by `rosen --json`. MCP
/// `structuredContent` must be a JSON object, so a single line is returned
/// directly and multi-line output (such as replayed offline snapshots) is
/// wrapped in `{ "items": [...] }`.
fn parse_structured(stdout: &str) -> Option<Value> {
    let mut objects = Vec::new();
    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let value: Value = serde_json::from_str(line).ok()?;
        if !value.is_object() {
            return None;
        }
        objects.push(value);
    }
    match objects.len() {
        0 => None,
        1 => objects.pop(),
        _ => Some(json!({ "items": objects })),
    }
}

/// The answer to `initialize`. The tools are the same in every protocol
/// revision so far, so the client's revision is accepted as it is.
fn initialized(params: &Value, managed: bool) -> Value {
    let version = params.get("protocolVersion").and_then(Value::as_str);
    let instructions = if managed {
        MANAGED_INSTRUCTIONS
    } else {
        INSTRUCTIONS
    };
    json!({
        "protocolVersion": version.unwrap_or(PROTOCOL_VERSION),
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "rosen", "version": env!("CARGO_PKG_VERSION") },
        "instructions": instructions,
    })
}

fn refusal(id: Value, code: i64, why: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": why } })
}

/// A host with no console of its own, such as a desktop app, would otherwise
/// get a console window flashing up for every call.
#[cfg(windows)]
fn without_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW`.
    const NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(NO_WINDOW);
}

#[cfg(not(windows))]
fn without_console_window(_command: &mut Command) {}
