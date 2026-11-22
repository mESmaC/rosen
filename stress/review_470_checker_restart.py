"""Replay the R12 no-checker timer across a fresh checker registry."""
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory

core = Path("crates/rosen-core/src/platform.rs").read_text(encoding="utf-8")
root = Path("crates/rosen-server/src/post_land")
checkers = (root / "checkers.rs").read_text(encoding="utf-8")
status = (root / "status.rs").read_text(encoding="utf-8")

def between(source, start, end):
    first = source.index(start)
    return source[first:source.index(end, first)]

platform = between(core, "use std::fmt;", "impl Serialize")
checkers = checkers[checkers.index("use rosen_core::Platform;"):]
checkers = checkers.replace("use rosen_core::Platform;", "use crate::Platform;")
checkers = checkers.replace("use rosen_protocol::ConnectedChecker;", "use crate::ConnectedChecker;")
timer = between(status, "pub const NO_CHECKER_WAIT_MS", "fn read_platforms")
timer += between(status, "fn waiting_no_checker(", "fn read_tables")
timer = timer.replace("rosen_protocol::Checkpoint", "Checkpoint")
program = platform + """
struct ConnectedChecker { machine: String, platform: String }
mod submitter {
    pub fn now_millis() -> i64 { 12_000_000 }
}
mod checkers {
""" + checkers + """
}
mod platforms {
    #[derive(Clone, Copy)]
    pub struct ReadTables;
    pub fn oldest_waiting_at(
        conn: &super::status::Connection, _: &str, _: &str,
        _: super::status::CheckKind, _: ReadTables, _: i64,
    ) -> Result<Option<i64>, ()> {
        Ok(Some(conn.oldest_waiting))
    }
}
mod status {
    use crate::Platform;
    use crate::checkers::CheckerRegistry;
    use std::sync::Arc;
    struct ServerState { checkers: Arc<CheckerRegistry> }
    pub struct Connection { pub oldest_waiting: i64 }
    struct Checkpoint { at: Option<i64> }
    #[derive(Clone, Copy)]
    pub enum CheckKind { Build, Test }
    type Result<T> = std::result::Result<T, ()>;
    fn internal(error: ()) { error }
""" + timer + """
    pub fn run() {
        let target = Platform::parse("linux").unwrap();
        let online = Platform::parse("linux-x86_64").unwrap();
        let now = super::submitter::now_millis();
        let conn = Connection { oldest_waiting: now - 2 * NO_CHECKER_WAIT_MS };
        let mut state = ServerState { checkers: Arc::new(CheckerRegistry::default()) };
        fn waiting(state: &ServerState, conn: &Connection, target: &Platform, now: i64) -> Option<i64> {
            waiting_no_checker(
                state, conn, "main", target, false, true, None, None,
                super::platforms::ReadTables, now,
            ).unwrap()
        }
        let guard = state.checkers.register("checker".into(), online);
        assert_eq!(state.checkers.matching(&target).len(), 1);
        drop(guard);
        assert_eq!(waiting(&state, &conn, &target, now), None);
        state.checkers = Arc::new(CheckerRegistry::default());
        let reported = waiting(&state, &conn, &target, now + 1);
        eprintln!("fresh restart at {now}; previous disconnect at {now}; alert = {reported:?}");
        assert_eq!(reported, None, "checker has been offline for only 1 ms");
    }
}
fn main() { status::run(); }
"""

with TemporaryDirectory() as folder:
    temp = Path(folder)
    source = temp / "restart.rs"
    source.write_text(program, encoding="utf-8")
    binary = temp / "restart.exe"
    subprocess.run(["rustc", "--edition=2021", "-Awarnings", str(source), "-o", str(binary)], check=True)
    raise SystemExit(subprocess.run([str(binary)]).returncode)
