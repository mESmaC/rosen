"""Exercise Rosen's R12 offer routing with an advisory-only check set."""
import argparse
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--root", default=".")
args = parser.parse_args()
root = Path(args.root)
core = (root / "crates/rosen-core/src/project_config.rs").read_text(encoding="utf-8")
platform = (root / "crates/rosen-core/src/platform.rs").read_text(encoding="utf-8")
server = (root / "crates/rosen-server/src/post_land.rs").read_text(encoding="utf-8")

def between(source, start, end):
    first = source.index(start)
    return source[first:source.index(end, first)]

multiple = "enum OfferTargets" in server
start = "struct CandidateTarget" if multiple else "enum OfferTarget"
end = "/// Selects the next pending" if multiple else "fn job_at"
routing = between(server, start, end).replace("rosen_core::ProjectConfig", "ProjectConfig")
platform = between(platform, "use std::fmt;", "impl Serialize")
methods = between(core, "    pub fn commands_for", "    fn validate(&self, key")
target_assertion = """
    let OfferTargets::Platforms(targets) = offer else {
        panic!("matching Linux checker received no separate advisory job");
    };
    assert!(targets.iter().any(|target| !target.required && target.platform.as_str() == "linux"));
""" if multiple else """
    let OfferTarget::Platform(target) = offer else {
        panic!("matching Linux checker received no separate advisory job");
    };
    assert_eq!(target.as_str(), "linux");
"""
resolve = "resolve_targets" if multiple else "resolve_target"
code = platform + """
use std::collections::BTreeMap;
#[derive(Default)]
struct CheckSet {
    platforms: Vec<Platform>,
    commands: Vec<String>,
    platform_commands: BTreeMap<Platform, Vec<String>>,
}
impl CheckSet {
""" + methods + """
}
struct ProjectConfig { post_land: CheckSet }
#[derive(Clone, Copy)]
enum CheckKind { Test }
fn check_set_for(config: &ProjectConfig, _: CheckKind) -> &CheckSet { &config.post_land }
""" + routing + """
fn main() {
    let linux = Platform::parse("linux").unwrap();
    let host = Platform::parse("linux-x86_64").unwrap();
    let mut config = ProjectConfig { post_land: CheckSet::default() };
    config.post_land.commands.push("shared tests".into());
    config.post_land.platform_commands.insert(linux.clone(), vec!["linux tests".into()]);
    assert_eq!(config.post_land.matching_target(&host), Some(&linux));
    let offer = """ + resolve + """(Some(&config), CheckKind::Test, Some(&host));
""" + target_assertion + """
}
"""

with TemporaryDirectory() as folder:
    temp = Path(folder)
    source = temp / "routing.rs"
    source.write_text(code, encoding="utf-8")
    binary = temp / "routing.exe"
    subprocess.run(["rustc", "--edition=2021", "-Awarnings", str(source), "-o", str(binary)], check=True)
    raise SystemExit(subprocess.run([str(binary)]).returncode)
