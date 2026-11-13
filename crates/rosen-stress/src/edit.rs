//! The edits one change makes in its local workspace, as an agent's tools
//! would: rewrite one line of the unit's shared file in place, and add a
//! new file.

use crate::plan::Plan;
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Writes change `n`'s edits into the workspace at `root` and returns the
/// `(path, line)` pairs the base must hold once it lands.
pub fn write_edits(
    plan: &Plan,
    agent: usize,
    n: usize,
    root: &Path,
) -> Result<Vec<(String, String)>> {
    let line = plan.edit_line(agent, n);
    let slots = Plan::slots_path(plan.unit_of(agent));
    rewrite_slot(
        &root.join(&slots),
        Plan::slot_line_index(plan.slot_of(agent, n)),
        &line,
    )?;
    let own = plan.own_path(agent, n);
    let own_file = root.join(&own);
    std::fs::create_dir_all(own_file.parent().expect("own path has a directory"))?;
    std::fs::write(&own_file, format!("{line}\n"))?;
    Ok(vec![(slots, line.clone()), (own, line)])
}

fn rewrite_slot(file: &Path, index: usize, line: &str) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| file.display().to_string())?;
    let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some(current) = lines.get(index) else {
        bail!(
            "{} has no line {index}; was it seeded for this plan?",
            file.display()
        );
    };
    if !current.starts_with("slot ") {
        bail!("{} line {index} is not a slot: {current:?}", file.display());
    }
    let replacement = format!("{line}\n");
    lines[index] = &replacement;
    std::fs::write(file, lines.concat())?;
    Ok(())
}
