//! `rosen land` and `rosen land --check` (SPEC R8 step 2, R10).

use crate::server::Succeeded;
use rosen_client::protocol::{
    ApprovalGate, LandAnswer, LandCheck, LandReach, LandStatus, LandedSince, LineEndingRewrite,
    MergeGate, ReceiptBinding, ReceiptGate,
};
use rosen_client::{Endpoint, Error};
use serde_json::json;

use crate::output::{LAND_CHECK_SCHEMA, LAND_SCHEMA};

/// Exits unsuccessfully when the land is refused, so scripts can branch on
/// the exit code alone.
pub async fn run(json: bool, endpoint: &Endpoint, change: &str) -> Result<Succeeded, Error> {
    let answer = rosen_client::land_reconnecting(endpoint, change).await?;
    print_answer(json, &answer);
    Ok(!matches!(answer.outcome, LandStatus::Refused { .. }))
}

fn print_answer(json: bool, answer: &LandAnswer) {
    if json {
        println!("{}", json!({ "schema": LAND_SCHEMA, "land": answer }));
        return;
    }
    let base_ref = &answer.base_ref;
    match &answer.outcome {
        LandStatus::Landed {
            commit,
            attempts,
            line_endings,
        } => {
            println!("landed change {} as {commit} on {base_ref}", answer.change);
            if *attempts > 1 {
                println!("({attempts} attempts: other lands moved {base_ref} first)");
            }
            if !line_endings.is_empty() {
                println!(
                    "line endings rewritten: {}",
                    line_endings_text(line_endings)
                );
            }
        }
        LandStatus::AlreadyLanded { commit } => {
            println!("change {} already landed as {commit}", answer.change);
        }
        LandStatus::Refused {
            reason, message, ..
        } => {
            eprintln!("land refused ({reason}): {message}");
            eprintln!("{base_ref} is at {}", answer.base);
        }
    }
}

/// `rosen land --check`: every gate a land would apply, without landing.
/// Exits unsuccessfully when the change would not land now.
pub async fn check(json: bool, endpoint: &Endpoint, change: &str) -> Result<Succeeded, Error> {
    let client = rosen_client::connect_or_start(endpoint).await?;
    let check = client.land_check(change).await?;
    if json {
        println!("{}", json!({ "schema": LAND_CHECK_SCHEMA, "check": check }));
    } else {
        print_check(&check);
    }
    Ok(check.landable)
}

fn print_check(check: &LandCheck) {
    println!(
        "change {} on {} at {} (head {})",
        check.change, check.base_ref, check.base, check.head
    );
    println!(
        "review:   {} · recheck {} · tier {} · reach {}",
        check.review_mode,
        check.recheck_mode,
        check.tier,
        reach_text(&check.reach)
    );
    for (gate, verdict) in gate_lines(check) {
        println!("{gate:<9} {verdict}");
    }
    for layer in &check.stack {
        let blocked = layer.blocked.as_deref().unwrap_or("ok");
        println!(
            "  {} {} {} ({blocked})",
            layer.change, layer.state, layer.tier
        );
    }
    if check.landable {
        println!("landable: yes");
    } else if let Some(refusal) = &check.refusal {
        println!("landable: no ({}): {}", refusal.reason, refusal.message);
    } else {
        println!("landable: pending (registered regeneration runs at land)");
    }
}

/// One line per gate, for `rosen land --check` and `rosen status`.
pub fn gate_lines(check: &LandCheck) -> Vec<(&'static str, String)> {
    let stack = match check.stack.len() {
        0 => "no unlanded ancestors".to_owned(),
        1 => "1 unlanded ancestor lands with it".to_owned(),
        n => format!("{n} unlanded ancestors land with it"),
    };
    let mut lines = vec![("merge:", merge_text(&check.merge))];
    if !check.line_endings.is_empty() {
        lines.push(("newlines:", line_endings_text(&check.line_endings)));
    }
    lines.extend([
        ("approval:", approval_text(&check.approval)),
        ("receipt:", receipt_text(&check.receipt)),
        ("stack:", stack),
    ]);
    lines
}

/// Each file a land writes in other line endings than the change gave it,
/// with the newline it lands with (R5, #424).
fn line_endings_text(rewrites: &[LineEndingRewrite]) -> String {
    let listed: Vec<_> = rewrites
        .iter()
        .map(|r| format!("{} ({})", r.path, r.eol))
        .collect();
    listed.join(", ")
}

pub fn reach_text(reach: &LandReach) -> String {
    match reach {
        LandReach::All(_) => "all".into(),
        LandReach::Units(units) if units.is_empty() => "none".into(),
        LandReach::Units(units) => units.join(", "),
    }
}

fn merge_text(merge: &MergeGate) -> String {
    match merge {
        MergeGate::Clean => "clean".into(),
        MergeGate::Conflicts { conflicts } => {
            let listed: Vec<_> = conflicts
                .iter()
                .map(|c| format!("{} {}", c.id, c.path))
                .collect();
            format!(
                "blocked: {} stored conflict(s): {}",
                conflicts.len(),
                listed.join(", ")
            )
        }
        MergeGate::MergeConflict { change, paths } => {
            format!("conflicts merging {change}: {}", paths.join(", "))
        }
        MergeGate::RegenerationRequired {
            change,
            paths,
            merged_tree,
            commands,
            pending_at_land,
        } => {
            let tree = merged_tree
                .map(|id| format!(" on {id}"))
                .unwrap_or_default();
            let cmds = if commands.is_empty() {
                String::new()
            } else {
                let list: Vec<String> = commands
                    .iter()
                    .map(|(path, cmd)| format!("{path}: `{cmd}`"))
                    .collect();
                format!(" ({})", list.join("; "))
            };
            let label = if *pending_at_land {
                "pending regeneration at land in"
            } else {
                "regenerate in"
            };
            format!("{label} {change}{tree}: {}{cmds}", paths.join(", "))
        }
        MergeGate::OversizeFiles { limit, files } => {
            let listed: Vec<_> = files
                .iter()
                .map(|f| format!("{} ({} bytes)", f.path, f.bytes))
                .collect();
            format!("over max_file_size={limit}: {}", listed.join(", "))
        }
        MergeGate::ConflictMarkers { paths } => {
            format!("conflict markers in: {}", paths.join(", "))
        }
        MergeGate::NotChecked => "not checked".into(),
    }
}

fn approval_text(approval: &ApprovalGate) -> String {
    match approval {
        ApprovalGate::Held { reviewers, head } => {
            let names: Vec<_> = reviewers.iter().map(|r| r.as_str()).collect();
            format!("held by {} for {head}", names.join(", "))
        }
        ApprovalGate::Needed => "needed for the current head".into(),
        ApprovalGate::NotRequired => "not required".into(),
    }
}

fn receipt_text(receipt: &ReceiptGate) -> String {
    let bound = |r: &ReceiptBinding| {
        format!(
            "`{}` on {} at {} by {}",
            r.check_set, r.head, r.base, r.submitter
        )
    };
    let commits = |lands: &[LandedSince]| {
        let named: Vec<_> = lands
            .iter()
            .map(|l| {
                l.change
                    .map_or_else(|| l.commit.to_string(), |c| c.to_string())
            })
            .collect();
        named.join(", ")
    };
    match receipt {
        ReceiptGate::Valid { receipt } => format!("valid: {}", bound(receipt)),
        ReceiptGate::Reusable {
            receipt,
            landed_since,
        } => format!(
            "reusable: {} across {}",
            bound(receipt),
            commits(landed_since)
        ),
        ReceiptGate::Stale {
            receipt,
            intersecting,
        } => format!("stale: {} meets {}", bound(receipt), commits(intersecting)),
        ReceiptGate::Unbound => "none bound to the current head and check set".into(),
        ReceiptGate::Missing => "missing".into(),
        ReceiptGate::NotRequired => "not required".into(),
        ReceiptGate::NotChecked => "not checked".into(),
    }
}
