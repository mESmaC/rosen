//! Human output for `rosen machine list` (SPEC R14): one line per machine,
//! with the credential state an owner needs to spot a machine that stopped
//! rotating or a pending request about to lapse.
use crate::history_cmd::utc_date;
use rosen_client::protocol::machine::{MachineRecord, MachineRole, MachineStatus};
use std::time::{SystemTime, UNIX_EPOCH};

const DAY: u64 = 86_400;
/// The daemon renews during a credential's final seven days (R14).
const RENEWAL_WINDOW: u64 = 7 * DAY;
/// An unapproved request is removed 7 days after it was made (R14).
const PENDING_EXPIRY: u64 = 7 * DAY;

pub fn print(records: &[MachineRecord]) {
    if records.is_empty() {
        println!("no machines");
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for record in records {
        println!("{}", line(record, now));
    }
}

fn line(record: &MachineRecord, now: u64) -> String {
    let role = match record.role {
        MachineRole::Agent => "agent",
        MachineRole::Checker => "checker",
    };
    let agents = if record.running_agents.is_empty() {
        "no agents".to_owned()
    } else {
        format!("agents {}", record.running_agents.join(", "))
    };
    format!(
        "{}  {}  {role}  {}  last contact {}  {agents}",
        record.id,
        record.name,
        credential(record, now),
        utc_minute(record.last_contact),
    )
}

/// The machine's status with its credential or request deadline.
fn credential(record: &MachineRecord, now: u64) -> String {
    match record.status {
        MachineStatus::Revoked => "revoked".into(),
        MachineStatus::Pending => match record.requested_at {
            Some(requested) => format!(
                "pending since {}, expires in {}",
                utc_minute(requested),
                span(requested.saturating_add(PENDING_EXPIRY).saturating_sub(now)),
            ),
            // A server from before #408 does not send the request time.
            None => "pending".into(),
        },
        MachineStatus::Active => match record.credential_expires_at {
            // A healthy daemon renews as the window opens, so a credential
            // still inside it means rotation has stalled.
            Some(expires) if expires.saturating_sub(now) <= RENEWAL_WINDOW => format!(
                "active, renewal overdue, expires in {}",
                span(expires.saturating_sub(now))
            ),
            Some(expires) => format!(
                "active, rotates in {}",
                span(expires - RENEWAL_WINDOW - now)
            ),
            None => "active, credential expired or not reported".into(),
        },
    }
}

/// A coarse duration: whole days, else hours, else minutes.
fn span(seconds: u64) -> String {
    if seconds >= DAY {
        format!("{} d", seconds / DAY)
    } else if seconds >= 3600 {
        format!("{} h", seconds / 3600)
    } else {
        format!("{} min", seconds / 60)
    }
}

/// `YYYY-MM-DD HH:MM UTC`; R14 reports last contact to the minute.
pub(crate) fn utc_minute(seconds: u64) -> String {
    let seconds = i64::try_from(seconds).unwrap_or(i64::MAX);
    let of_day = seconds.rem_euclid(DAY as i64);
    format!(
        "{} {:02}:{:02} UTC",
        utc_date(seconds),
        of_day / 3600,
        of_day % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Risk (#408): a stalled rotation or a lapsing request must read as
    /// such, or the owner misses a machine that is offline for good.
    #[test]
    fn credential_column_shows_rotation_overdue_and_pending_deadline() {
        let now = 1_800_000_000;
        let record = |status, credential_expires_at, requested_at| MachineRecord {
            id: "m".into(),
            name: "m".into(),
            role: MachineRole::Agent,
            status,
            last_contact: now,
            running_agents: Vec::new(),
            credential_expires_at,
            requested_at,
        };
        let fresh = record(MachineStatus::Active, Some(now + 13 * DAY), None);
        assert_eq!(credential(&fresh, now), "active, rotates in 6 d");
        let stalled = record(MachineStatus::Active, Some(now + 3 * DAY), None);
        assert_eq!(
            credential(&stalled, now),
            "active, renewal overdue, expires in 3 d"
        );
        let asked = record(MachineStatus::Pending, None, Some(now - 120));
        assert_eq!(
            credential(&asked, now),
            "pending since 2027-01-15 07:58 UTC, expires in 6 d"
        );
    }
}
