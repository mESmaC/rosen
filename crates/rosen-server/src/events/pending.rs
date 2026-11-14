//! Events whose write failed, kept in publish order until they are stored
//! (SPEC R9: delivery is at least once).
//!
//! A producer publishes after it has committed the change the event reports,
//! so a failed write cannot undo that change. Dropping the event would break
//! at-least-once delivery: a released claim's waiter would never wake (R6).
//! So a failed event waits here with the audience it was given at publish,
//! and every later event queues behind it, which keeps a release and the
//! land that caused it in order. A background thread retries until the
//! queue is empty.

use super::{store, EventError, Inner};
use rosen_protocol::EventKind;
use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

/// First pause before retrying a failed write; it doubles up to the cap.
const FIRST_RETRY: Duration = Duration::from_millis(100);
const MAX_RETRY: Duration = Duration::from_secs(5);

/// One event waiting to be stored, with its audience already fixed.
pub(super) struct Pending {
    pub kind: EventKind,
    pub change: Option<String>,
    pub detail: serde_json::Value,
    /// `None` broadcasts the event.
    pub audience: Option<BTreeSet<String>>,
    /// The outbox row a staged event is moved from, if it was staged.
    pub outbox: Option<i64>,
}

/// Stores queued events oldest first, stopping at the first failure so none
/// is stored out of order. Returns the sequence number of the last event
/// stored, or 0 when none was.
pub(super) fn flush(
    inner: &Inner,
    conn: &mut rusqlite::Connection,
    queue: &mut VecDeque<Pending>,
) -> Result<u64, EventError> {
    let mut last = 0;
    while let Some(next) = queue.front() {
        let seq = store::append(
            conn,
            next.kind,
            next.change.as_deref(),
            &next.detail,
            next.audience.as_ref(),
            next.outbox,
        )?;
        queue.pop_front();
        // `None`: a racing delivery already stored this staged event.
        if let Some(seq) = seq {
            // Announce while the log's lock is held, so subscribers see
            // sequence numbers in order.
            inner.latest.send_replace(seq);
            last = seq;
        }
    }
    Ok(last)
}

/// Starts the retry thread unless one is already running. Call with the
/// log's lock held, after a flush left events queued.
pub(super) fn ensure_retrying(inner: &Arc<Inner>) {
    if inner.retrying.swap(true, Ordering::AcqRel) {
        return;
    }
    let weak = Arc::downgrade(inner);
    let spawned = std::thread::Builder::new()
        .name("rosen-event-retry".into())
        .spawn(move || retry_until_stored(weak));
    if let Err(err) = spawned {
        // The next publish flushes the queue first, so nothing is lost; it
        // only waits for that publish.
        eprintln!("rosen-server: could not start the event retry thread: {err}");
        inner.retrying.store(false, Ordering::Release);
    }
}

/// Retries the queue with backoff until it is empty or the log is dropped.
fn retry_until_stored(log: Weak<Inner>) {
    let mut delay = FIRST_RETRY;
    loop {
        std::thread::sleep(delay);
        let Some(inner) = log.upgrade() else {
            return;
        };
        let drained = inner.with_slot(|slot| {
            let result = match inner.connection(&mut slot.conn, true) {
                Ok(Some(conn)) => flush(&inner, conn, &mut slot.queue).map(drop),
                Ok(None) => Ok(()),
                Err(err) => Err(err),
            };
            if let Err(err) = result {
                let unstored = slot.queue.len();
                eprintln!("rosen-server: retrying {unstored} unstored events: {err}");
            }
            // Cleared under the lock: a publish that fails after this sees
            // no retry thread and starts one.
            let empty = slot.queue.is_empty();
            if empty {
                inner.retrying.store(false, Ordering::Release);
            }
            empty
        });
        if drained {
            return;
        }
        delay = (delay * 2).min(MAX_RETRY);
    }
}
