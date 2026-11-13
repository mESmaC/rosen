//! `rosen-stress seed`: the git repository a stress project is imported
//! from. It holds `rosen.toml` with one build unit per unit directory, and
//! in each unit the shared slots file and the contended path.

use crate::plan::Plan;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// Writes the seed repository into `dir`, which must not exist yet, and
/// commits it on branch `main`.
pub fn write_seed(plan: &Plan, dir: &Path) -> Result<()> {
    if dir.exists() {
        bail!(
            "{} already exists; seed into a new directory",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("rosen.toml"), plan.seed_config())?;
    let slots = plan.seed_slots_file();
    for unit in 0..plan.units {
        let unit_dir = dir.join(Plan::unit_dir(unit));
        std::fs::create_dir_all(&unit_dir)?;
        std::fs::write(dir.join(Plan::slots_path(unit)), &slots)?;
        std::fs::write(dir.join(Plan::hot_path(unit)), b"contended\n")?;
    }
    git(dir, &["init", "--quiet", "--initial-branch=main"])?;
    git(dir, &["add", "--all"])?;
    git(dir, &["commit", "--quiet", "--message", "stress seed"])?;
    Ok(())
}

/// Runs git with a fixed identity and no line-ending conversion, so the
/// seed's bytes are the ones every agent's slot arithmetic expects.
fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "user.name=rosen-stress",
            "-c",
            "user.email=stress@rosen.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .context("running git")?;
    if !status.success() {
        bail!("git {} failed with {status}", args.join(" "));
    }
    Ok(())
}
