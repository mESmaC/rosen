//! R10 contract: a missing schema or non-JSON failure breaks agent automation.
use serde_json::Value;
use std::process::Command;

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
