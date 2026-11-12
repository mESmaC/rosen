//! Log and path history. The log follows first parents: landed history is
//! one commit per change on a single line (D15), and for imported merges
//! the first parent is the branch merged into. A change's log is its own
//! snapshots, newest first, then those of the changes below it in its
//! stack, then the base's log from the commit the stack sits on.

use super::index::CommitInfo;
use super::revision::{stack_root, Revision};
use super::trees::{normalise, selected};
use super::{Ctx, HistoryResult};
use rosen_protocol::{Log, LogEntry, LogRequest};

const DEFAULT_LIMIT: u32 = 100;
/// Caps one answer, so a single frame stays a sensible size.
const MAX_LIMIT: u32 = 10_000;

pub fn log(ctx: &Ctx, request: &LogRequest) -> HistoryResult<Log> {
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let filters = request
        .paths
        .iter()
        .map(|p| normalise(p))
        .collect::<HistoryResult<Vec<_>>>()?;
    let mut walk = Walk {
        ctx,
        filters: &filters,
        // One past the limit tells whether the log was cut short.
        wanted: limit + 1,
        entries: Vec::new(),
    };
    let start = match Revision::resolve(ctx, request.at.as_deref())? {
        Revision::Change(change) => {
            let (stack, base) = stack_root(ctx, &change)?;
            for change in &stack {
                for snapshot in change.snapshots.iter().rev() {
                    let mut info = ctx.index.commit(&ctx.store, &snapshot.commit)?;
                    info.change = Some(change.id);
                    walk.offer(&info)?;
                }
            }
            base
        }
        other => other.history_commit(ctx)?,
    };
    let mut next = Some(start);
    while let Some(commit) = next.filter(|_| !walk.full()) {
        let info = ctx.index.commit(&ctx.store, &commit)?;
        walk.offer(&info)?;
        next = info.first_parent();
    }
    let mut entries = walk.entries;
    let truncated = entries.len() > limit;
    entries.truncate(limit);
    Ok(Log { entries, truncated })
}

struct Walk<'a> {
    ctx: &'a Ctx<'a>,
    filters: &'a [String],
    wanted: usize,
    entries: Vec<LogEntry>,
}

impl Walk<'_> {
    fn full(&self) -> bool {
        self.entries.len() >= self.wanted
    }

    /// Keeps the commit if it changed a selected path, read from the path
    /// index (derived on first use).
    fn offer(&mut self, info: &CommitInfo) -> HistoryResult<()> {
        if self.full() {
            return Ok(());
        }
        let keep = self.filters.is_empty() || {
            let paths = self.ctx.index.changed_paths(&self.ctx.store, info)?;
            paths.iter().any(|path| selected(path, self.filters))
        };
        if keep {
            self.entries.push(info.entry());
        }
        Ok(())
    }
}
