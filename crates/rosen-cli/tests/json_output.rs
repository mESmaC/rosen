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
    for line in include_str!("../src/output.rs").lines() {
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
