//! Risk (SPEC §2, R8, #367): broad input invalidation wastes every pending
//! receipt; missing a consumer lets unchecked dependent code land.
use super::tests::{commit, nested};
use super::*;
use rosen_core::{AgentId, ChangeId, ChangeParent, ChangeState};
use rosen_store::MemoryObjectStore;

type Sources = BTreeMap<&'static str, String>;

fn sources(case: &str, edited: bool) -> Sources {
    let mut files: Sources = [
        ("Cargo.toml", "[workspace]\nmembers=['crates/*']\n[workspace.dependencies]\nalias={package='ext',version='1'}\n".into()),
        ("crates/a/Cargo.toml", "[package]\nname='a'\nversion='0.1.0'\n".into()),
        ("crates/b/Cargo.toml", "[package]\nname='b'\nversion='0.1.0'\n[dependencies]\nalias={workspace=true}\n".into()),
        ("crates/a/src/lib.rs", "a".into()),
        ("crates/b/src/lib.rs", "b".into()),
        ("rosen.toml", if case == "ci_input" { "workspace_inputs=['.github/workflows/']" } else { "" }.into()),
    ].into();
    ownership_sources(&mut files, case);
    if case.starts_with("lock_") {
        let a = if matches!(case, "lock_shared" | "lock_versions") {
            "[dependencies]\next='1'\n"
        } else {
            ""
        };
        let b = match case {
            "lock_shared" => "",
            "lock_versions" => "[dependencies]\next='2'\n",
            "lock_transitive" => "[dependencies]\nmid='1'\n",
            _ => "[dependencies]\next='1'\n",
        };
        files.insert(
            "crates/a/Cargo.toml",
            format!("[package]\nname='a'\nversion='0.1.0'\n{a}"),
        );
        files.insert(
            "crates/b/Cargo.toml",
            format!("[package]\nname='b'\nversion='0.1.0'\n{b}"),
        );
        files.insert("Cargo.lock", lockfile(case, edited));
    }
    if edited {
        edit(&mut files, case);
    }
    files
}

fn ownership_sources(files: &mut Sources, case: &str) {
    match case {
        "configured_unit_names" => {
            files.insert(
                "rosen.toml",
                "[build_units.a]\npaths=['crates/b/**']\n[build_units.b]\npaths=['crates/a/**']\n"
                    .into(),
            );
        }
        "configured_multiple_holders" => {
            files.insert(
                "rosen.toml",
                "[build_units.a]\npaths=['crates/a/**']\n[build_units.b]\npaths=['crates/a/**']\n"
                    .into(),
            );
        }
        "implicit_workspace_member" => {
            files.insert("Cargo.toml", "[workspace]\nmembers=['crates/a']\n[workspace.dependencies]\nalias={package='ext',version='1'}\n".into());
            files.insert(
                "crates/a/Cargo.toml",
                "[package]\nname='a'\nversion='0.1.0'\n[dependencies]\nhelper={path='../helper'}\n"
                    .into(),
            );
            files.insert("crates/helper/Cargo.toml", "[package]\nname='helper'\nversion='0.1.0'\n[dependencies]\nalias={workspace=true}\n".into());
        }
        _ => {}
    }
}

fn edit(files: &mut Sources, case: &str) {
    let config = match case {
        "config_operations" => Some("ignore=['tmp/']\nsensitive=['secret/']\nreview='after-land'\n[prepare]\ncommands=['echo ok']\n[warm_pool]\nsize=2\n[mirror]\nremote='https://example.com/repo'"),
        "config_checks" => Some("[checks.pre_land]\ncommands=['cargo build']"),
        "config_units" => Some("[build_units.a]\npaths=['crates/a/**']"),
        "config_inputs" => Some("workspace_inputs=['shared/']"),
        "config_generated" => Some("generated=['out/']"),
        "config_registration" => Some("registration_lists=['list.rs']"),
        "config_invalid" => Some("unknown=true"),
        _ => None,
    };
    if let Some(config) = config {
        files.insert("rosen.toml", config.into());
    }
    match case {
        "ci" | "ci_input" => {
            files.insert(".github/workflows/ci.yml", "jobs: {}".into());
        }
        "workspace_alias" | "implicit_workspace_member" => {
            let root = files.get_mut("Cargo.toml").unwrap();
            *root = root.replace("version='1'", "version='2'");
        }
        "member_manifest" => {
            files
                .get_mut("crates/b/Cargo.toml")
                .unwrap()
                .push_str("# edit\n");
        }
        "configured_unit_names" | "configured_multiple_holders" => {
            let path = if case == "configured_unit_names" {
                "crates/b/Cargo.toml"
            } else {
                "crates/a/Cargo.toml"
            };
            let manifest = files.get_mut(path).unwrap();
            *manifest = manifest.replace("version='0.1.0'", "version='0.2.0'");
        }
        "toolchain" => {
            files.insert(
                "rust-toolchain.toml",
                "[toolchain]\nchannel='stable'".into(),
            );
        }
        _ => {}
    }
}

fn lockfile(case: &str, edited: bool) -> String {
    if case == "lock_invalid" && edited {
        return "not a lockfile [".into();
    }
    let a = if case == "lock_shared" || case == "lock_versions" {
        "['ext 1.0.0']"
    } else {
        "[]"
    };
    let b = match case {
        "lock_shared" => "[]",
        "lock_transitive" => "['mid']",
        "lock_versions" => "['ext 2.0.0']",
        _ => "['ext 1.0.0']",
    };
    let checksum = if edited { "new" } else { "old" };
    let first = if case == "lock_versions" {
        "old"
    } else {
        checksum
    };
    let mut lock = format!("version=4\n[[package]]\nname='a'\nversion='0.1.0'\ndependencies={a}\n[[package]]\nname='b'\nversion='0.1.0'\ndependencies={b}\n[[package]]\nname='ext'\nversion='1.0.0'\nsource='registry+https://example.com/index'\nchecksum='{first}'\n");
    if case == "lock_transitive" {
        lock.push_str("[[package]]\nname='mid'\nversion='1.0.0'\ndependencies=['ext']\n");
    }
    if case == "lock_versions" {
        lock.push_str(&format!("[[package]]\nname='ext'\nversion='2.0.0'\nsource='registry+https://example.com/index'\nchecksum='{checksum}'\n"));
    }
    if case == "lock_independent" && edited {
        lock.replace("1.0.0", "1.1.0")
    } else {
        lock
    }
}

fn tree(store: &MemoryObjectStore, files: &Sources) -> ObjectId {
    let files: Vec<_> = files.iter().map(|(p, s)| (*p, s.as_str())).collect();
    nested(store, &files)
}

fn facts(root: ObjectId, head: ObjectId) -> ChangeFacts {
    ChangeFacts {
        id: ChangeId::from_bytes([1; 16]),
        owner: AgentId::new("test").unwrap(),
        description: "a".into(),
        state: ChangeState::Ready,
        parent: ChangeParent::Base(root),
        head,
        merge_base: root,
        tier: Tier::Normal,
        has_conflicts: false,
        approved_head: None,
        approvers: vec![],
    }
}

fn numbers(reached: &crate::landing::reach::Reach) -> String {
    reached
        .iter()
        .map(|s| match s.as_str() {
            "a" => "0",
            "b" | "helper" => "1",
            "" => "2",
            _ => panic!("unexpected unit"),
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",")
}

fn build_graph(store: &MemoryObjectStore, before: &Sources) -> BuildGraph {
    let config = ProjectConfig::parse(&before["rosen.toml"]).unwrap();
    let units = if config.build_units.is_empty() {
        cargo_units(store, &files(store, tree(store, before)).unwrap()).unwrap()
    } else {
        config.build_units.clone()
    };
    graph(units, &config)
}

#[test]
fn stored_input_reach_matches_proven_fixtures() {
    let table = include_str!("../../../../../proofs/fixtures/recheck.tsv");
    let mut checked = 0;
    for row in table.lines().skip(1) {
        let cells: Vec<_> = row.split('\t').collect();
        let Some(case) = cells.get(4).filter(|s| !s.is_empty()) else {
            continue;
        };
        checked += 1;
        let store = MemoryObjectStore::new();
        let (before, mut after) = (sources(case, false), sources(case, true));
        let root = commit(&store, tree(&store, &before), vec![]);
        let landed_tree = tree(&store, &after);
        let landed = commit(&store, landed_tree, vec![root]);
        let graph = build_graph(&store, &before);
        let reached = match inputs::reach(&store, &graph, &root, &landed, false) {
            Ok(reached) => reached,
            Err(Stop::Unreadable(_)) => graph.all(),
            Err(why) => panic!("{why:?}"),
        };
        assert_eq!(numbers(&reached), cells[1], "{case}");
        let code = if *case == "configured_unit_names" {
            "crates/b/src/lib.rs"
        } else {
            "crates/a/src/lib.rs"
        };
        assert_eq!(
            numbers(&graph.reach(&[code.into()], false)),
            cells[0],
            "{case}"
        );
        after.insert(code, "new code".into());
        let merged = tree(&store, &after);
        let mut own = before;
        own.insert(code, "new code".into());
        let facts = facts(root, tree(&store, &own));
        assert_eq!(
            reusable(
                &store,
                &UnitsCache::default(),
                &landed,
                &facts,
                &merged,
                &[(landed, Tier::Normal)]
            )
            .unwrap(),
            cells[3] == "1",
            "{case}"
        );
    }
    assert_eq!(checked, 20);
}
