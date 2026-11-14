//! `rosen mcp`: a Model Context Protocol server on stdin and stdout (R10).
//!
//! It offers the `rosen` commands an agent works with as tools, so an agent
//! calls them with named arguments instead of composing shell lines. Nothing
//! is built twice: the tools are read from the command definitions
//! (`mcp_tools`), and a call runs this same program with `--json`, so a
//! tool's result is that command's own output and follows its schema.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::commands::McpArgs;
use crate::mcp_tools::{self, Tool};

/// The protocol revision answered when the client names none.
const PROTOCOL_VERSION: &str = "2024-11-05";

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const PARSE_ERROR: i64 = -32700;

const INSTRUCTIONS: &str = "Rosen is this project's version control. Each tool runs the `rosen` \
command of the same name and returns its JSON output; a refusal comes back as an error with \
Rosen's reason. Every file written in a Rosen workspace is stored on the server within 2 \
seconds, so there is no add, push or pull. Tools that take an optional `change` act on this \
workspace's change when it is left out.";

/// A request's failure: a JSON-RPC error code and what went wrong.
type Refusal = (i64, String);

struct Server {
    tools: Vec<Tool>,
    /// This program, run again for each call.
    program: PathBuf,
    endpoint: Option<String>,
    workspace: Option<PathBuf>,
}

/// Serves requests, one JSON object per line, until stdin closes.
pub fn serve(args: &McpArgs, endpoint: Option<&str>) -> std::io::Result<()> {
    let server = Server {
        tools: mcp_tools::tools(args.landing),
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
            "initialize" => Ok(initialized(&params)),
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
        let (text, failed) = match tool.command_line(arguments) {
            Ok(words) => self.run(&words),
            Err(why) => (why, true),
        };
        Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": failed,
        }))
    }

    /// Runs `rosen --json <words>` and returns what it printed and whether
    /// it failed. Failures print a `rosen.error/v1` object like any other
    /// output; whatever went to stderr follows it.
    fn run(&self, words: &[String]) -> (String, bool) {
        let mut command = Command::new(&self.program);
        command.arg("--json");
        // Before the words: they can end with `--` and positional values.
        if let Some(endpoint) = &self.endpoint {
            command.arg(format!("--endpoint={endpoint}"));
        }
        command.args(words).stdin(Stdio::null());
        if let Some(workspace) = &self.workspace {
            command.current_dir(workspace);
        }
        without_console_window(&mut command);
        let output = match command.output() {
            Ok(output) => output,
            Err(error) => {
                let program = self.program.display();
                return (format!("could not run {program}: {error}"), true);
            }
        };
        let printed = String::from_utf8_lossy(&output.stdout);
        let errors = String::from_utf8_lossy(&output.stderr);
        let parts = [printed.trim_end(), errors.trim_end()];
        let text: Vec<&str> = parts.into_iter().filter(|part| !part.is_empty()).collect();
        (text.join("\n"), !output.status.success())
    }
}

/// The answer to `initialize`. The tools are the same in every protocol
/// revision so far, so the client's revision is accepted as it is.
fn initialized(params: &Value) -> Value {
    let version = params.get("protocolVersion").and_then(Value::as_str);
    json!({
        "protocolVersion": version.unwrap_or(PROTOCOL_VERSION),
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "rosen", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
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
