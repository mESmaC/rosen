//! R10 contract: a missing schema or non-JSON failure breaks agent automation.
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rosen"))
        .args(args)
        .output()
        .expect("run rosen")
}

fn check_refs(value: &Value, catalog: &Value) {
    match value {
        Value::Object(fields) => {
            if let Some(reference) = fields.get("$ref") {
                let pointer = reference.as_str().unwrap().strip_prefix('#').unwrap();
                assert!(catalog.pointer(pointer).is_some(), "missing {pointer}");
            }
            for field in fields.values() {
                check_refs(field, catalog);
            }
        }
        Value::Array(items) => {
            for item in items {
                check_refs(item, catalog);
            }
        }
        _ => {}
    }
}

#[test]
fn output_catalog_and_failures_stay_machine_readable() {
    let output = run(&["--json-schema"]);
    assert!(output.status.success());
    let catalog: Value = serde_json::from_slice(&output.stdout).unwrap();
    check_refs(&catalog, &catalog);
    for line in include_str!("../../src/output.rs").lines() {
        if let Some((_, tag)) = line.split_once(": &str = \"") {
            let tag = tag.trim_end_matches("\";");
            assert!(catalog["$defs"].get(tag).is_some(), "missing {tag}");
        }
    }
    for args in [
        vec!["--json"],
        vec!["--json", "change"],
        vec!["--json", "status", "--unknown"],
        vec!["resolve", "../file.txt", "--change", "k", "--json"],
    ] {
        let output = run(&args);
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["schema"], "rosen.error/v1");
        assert!(error["error"].is_string());
        assert_eq!(error["command"].is_null(), output.status.code() == Some(2));
    }
    let help = run(&["--json", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--json-schema"));
}

// Risk (R10): managed workspaces could still expose host-owned commands or
// stale rebase wording.
#[test]
fn managed_workspace_help_hides_host_commands_and_fixes_rebase() {
    let managed = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rosen"))
            .args(args)
            .env("ROSEN_CHANGE", "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk")
            .env("ROSEN_MANAGED_WORKSPACE", "1")
            .output()
            .expect("run rosen managed")
    };

    let help = String::from_utf8_lossy(&managed(&["--help"]).stdout).into_owned();
    assert!(help.contains("managed workspace"), "{help}");
    assert!(!help.contains("\n  land "), "{help}");
    assert!(!help.contains("\n  receipt "), "{help}");

    let ws_help = String::from_utf8_lossy(&managed(&["workspace", "--help"]).stdout).into_owned();
    assert!(!ws_help.contains("\n  delete "), "{ws_help}");

    let rebase_help = String::from_utf8_lossy(&managed(&["rebase", "--help"]).stdout).into_owned();
    assert!(
        !rebase_help.contains("with its watcher stopped"),
        "{rebase_help}"
    );

    let unmanaged = Command::new(env!("CARGO_BIN_EXE_rosen"))
        .args(["rebase", "--help"])
        .env_remove("ROSEN_MANAGED_WORKSPACE")
        .output()
        .expect("run rosen unmanaged");
    assert!(String::from_utf8_lossy(&unmanaged.stdout).contains("with its watcher stopped"));
}

// Risk (R10): MCP tools could omit wait/run/events, leave status fields
// undocumented, or return only unstructured text blocks.
#[test]
fn mcp_tools_match_cli_and_return_structured_content() {
    use std::io::Write;
    use std::process::Stdio;

    let (shell, flag, ok_cmd) = if cfg!(windows) {
        ("cmd", "/C", "echo ok")
    } else {
        ("sh", "-c", "echo ok")
    };
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_rosen"))
        .args(["mcp"])
        .env("ROSEN_CHANGE", "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk")
        .env("ROSEN_MANAGED_WORKSPACE", "1")
        .env("ROSEN_RUN_HELD", "build")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn rosen mcp");
    let requests = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run","arguments":{"command":[shell, flag, ok_cmd]}}}),
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"status","arguments":{"bogus":true}}}),
    ];
    {
        let mut stdin = mcp.stdin.take().unwrap();
        for req in &requests {
            writeln!(stdin, "{req}").unwrap();
        }
    }
    let output = mcp.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let replies: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 4);

    assert_mcp_tools(replies[1]["result"]["tools"].as_array().unwrap());
    assert_eq!(replies[2]["result"]["isError"], false, "{}", replies[2]);
    assert_eq!(
        replies[2]["result"]["structuredContent"]["schema"],
        "rosen.run/v1"
    );
    assert_eq!(replies[2]["result"]["structuredContent"]["success"], true);
    assert_eq!(replies[3]["result"]["isError"], true);
    assert_eq!(
        replies[3]["result"]["structuredContent"]["schema"],
        "rosen.error/v1"
    );
}

fn assert_mcp_tools(tools: &[Value]) {
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "wait", "run", "events", "status", "commit", "diff", "rebase",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    for hidden in [
        "land",
        "receipt_submit",
        "review_approve",
        "workspace_delete",
    ] {
        assert!(!names.contains(&hidden), "unexpected {hidden} in {names:?}");
    }
    let desc = |name: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["description"]
            .as_str()
            .unwrap()
    };
    for term in [
        "tier",
        "reach",
        "recheck_mode",
        "next_action",
        "actor",
        "affected_head",
    ] {
        assert!(desc("status").contains(term), "status desc missing {term}");
    }
    assert!(!desc("rebase").contains("with its watcher stopped"));
}

struct DaemonGuard(Option<Child>);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn server_bin() -> Option<PathBuf> {
    let name = format!("rosen-server{}", std::env::consts::EXE_SUFFIX);
    let candidate = Path::new(env!("CARGO_BIN_EXE_rosen")).parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

fn clean_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ROSEN_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn validate_schema(value: &Value, schema: &Value, catalog: &Value) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| format!("non-local $ref {reference}"))?;
        let target = catalog
            .pointer(pointer)
            .ok_or_else(|| format!("missing $ref {reference}"))?;
        return validate_schema(value, target, catalog);
    }
    if let Some(cases) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = cases
            .iter()
            .filter(|case| validate_schema(value, case, catalog).is_ok())
            .count();
        return (matches == 1)
            .then_some(())
            .ok_or_else(|| format!("oneOf matched {matches} branches for {value}"));
    }
    if let Some(cases) = schema.get("anyOf").and_then(Value::as_array) {
        return cases
            .iter()
            .any(|case| validate_schema(value, case, catalog).is_ok())
            .then_some(())
            .ok_or_else(|| format!("anyOf matched 0 branches for {value}"));
    }
    if let Some(expected) = schema.get("const") {
        if value != expected {
            return Err(format!("expected const {expected}, got {value}"));
        }
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array) {
        if !variants.contains(value) {
            return Err(format!("value {value} not in enum {variants:?}"));
        }
    }
    validate_type_and_contents(value, schema, catalog)
}

fn validate_type_and_contents(
    value: &Value,
    schema: &Value,
    catalog: &Value,
) -> Result<(), String> {
    if let Some(ty) = schema.get("type").and_then(Value::as_str) {
        let ok = match ty {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            other => return Err(format!("unsupported type {other}")),
        };
        if !ok {
            return Err(format!("expected type {ty}, got {value}"));
        }
    }
    if let Some(min) = schema.get("minimum").and_then(Value::as_i64) {
        if let Some(n) = value.as_i64() {
            if n < min {
                return Err(format!("value {n} < minimum {min}"));
            }
        }
    }
    if let Some(map) = value.as_object() {
        validate_object(map, schema, catalog)?;
    }
    if let (Some(items), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for item in items {
            validate_schema(item, item_schema, catalog)?;
        }
    }
    Ok(())
}

fn validate_object(
    map: &serde_json::Map<String, Value>,
    schema: &Value,
    catalog: &Value,
) -> Result<(), String> {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !map.contains_key(name) {
                return Err(format!("missing required property {name}"));
            }
        }
    }
    let props = schema.get("properties").and_then(Value::as_object);
    let forbid_extra = schema.get("additionalProperties") == Some(&Value::Bool(false));
    for (key, val) in map {
        if let Some(prop_schema) = props.and_then(|p| p.get(key)) {
            validate_schema(val, prop_schema, catalog)?;
        } else if forbid_extra {
            return Err(format!("undeclared property {key}"));
        }
    }
    Ok(())
}

fn wait_for_running_status(endpoint: &str, child: &mut Child) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(exit) = child.try_wait().unwrap() {
            panic!("rosen-server exited early ({exit})");
        }
        let out = clean_command(env!("CARGO_BIN_EXE_rosen"))
            .args(["--json", "--endpoint", endpoint, "server", "status"])
            .output()
            .unwrap();
        if out.status.success() {
            return serde_json::from_slice(&out.stdout).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("rosen-server did not become ready on {endpoint}");
}

// Risk (R10, #477): `rosen server status --json` emits `mirror_owner` (bool or
// null) while `rosen.server.status/v1` sets `additionalProperties: false`; a
// running daemon's output must validate against `--json-schema`.
#[test]
fn running_server_status_matches_json_schema() {
    let catalog: Value = serde_json::from_slice(&run(&["--json-schema"]).stdout).unwrap();
    let status_schema = &catalog["$defs"]["rosen.server.status/v1"];

    let dir = std::env::temp_dir().join(format!("rosen-st-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let endpoint = if cfg!(windows) {
        format!(r"\\.\pipe\rosen-status-schema-{}", std::process::id())
    } else {
        // Leave `sock/` uncreated so `rosen-server` creates it with mode 0700.
        dir.join("sock")
            .join("s.sock")
            .to_string_lossy()
            .into_owned()
    };

    let stopped = clean_command(env!("CARGO_BIN_EXE_rosen"))
        .args(["--json", "--endpoint", &endpoint, "server", "status"])
        .output()
        .unwrap();
    assert!(!stopped.status.success());
    let stopped_json: Value = serde_json::from_slice(&stopped.stdout).unwrap();
    validate_schema(&stopped_json, status_schema, &catalog).unwrap();
    validate_schema(&stopped_json, &catalog, &catalog).unwrap();

    if let Some(server) = server_bin() {
        let child = clean_command(server)
            .args(["--data-dir"])
            .arg(dir.join("data"))
            .args(["--endpoint", &endpoint])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn rosen-server");
        let mut guard = DaemonGuard(Some(child));

        let mut running = wait_for_running_status(&endpoint, guard.0.as_mut().unwrap());
        assert!(running["mirror_owner"].is_boolean(), "{running}");
        validate_schema(&running, status_schema, &catalog).unwrap();
        validate_schema(&running, &catalog, &catalog).unwrap();

        // Cache daemons emit `mirror_owner: null`; verify that validates too,
        // and that an undeclared property is still rejected.
        running["mirror_owner"] = Value::Null;
        validate_schema(&running, status_schema, &catalog).unwrap();
        running["undeclared_extra"] = Value::Bool(true);
        assert!(validate_schema(&running, status_schema, &catalog).is_err());

        let stop = clean_command(env!("CARGO_BIN_EXE_rosen"))
            .args(["--json", "--endpoint", &endpoint, "server", "stop"])
            .output()
            .unwrap();
        assert!(stop.status.success(), "{stop:?}");
        guard.0.take().unwrap().wait().unwrap();
    }
    let _ = fs::remove_dir_all(&dir);
}
