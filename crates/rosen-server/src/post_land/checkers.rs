//! Tracks live checker connections and their reported platforms (SPEC R12, #440).
//!
//! Each connected checker connection acquires a [`CheckerGuard`]. Dropping the
//! guard (when the connection closes) removes the checker immediately, so
//! `rosen server status`, `rosen review status` and the Collapse state feed
//! show coverage gaps without waiting for a timeout.

use rosen_core::Platform;
use rosen_protocol::ConnectedChecker;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Default)]
struct RegistryState {
    next_id: u64,
    version: u64,
    active: BTreeMap<u64, (String, Platform)>,
    /// Unix milliseconds when the most recent checker on each platform disconnected.
    last_offline: BTreeMap<Platform, i64>,
}

/// Live checker connections on the server, keyed by connection registration ID.
pub struct CheckerRegistry {
    started_at_ms: i64,
    inner: Mutex<RegistryState>,
}

impl Default for CheckerRegistry {
    fn default() -> Self {
        Self {
            started_at_ms: super::submitter::now_millis(),
            inner: Mutex::default(),
        }
    }
}

/// Keeps one checker connection registered until dropped.
pub struct CheckerGuard {
    registry: Arc<CheckerRegistry>,
    id: u64,
    machine: String,
    platform: Platform,
}

impl CheckerGuard {
    pub fn matches_entry(&self, machine: &str, platform: &Platform) -> bool {
        self.machine == machine && &self.platform == platform
    }
}

impl Drop for CheckerGuard {
    fn drop(&mut self) {
        let mut guard = self
            .registry
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if guard.active.remove(&self.id).is_some() {
            let now = super::submitter::now_millis();
            guard.last_offline.insert(self.platform.clone(), now);
            guard.version = guard.version.wrapping_add(1);
        }
    }
}

impl CheckerRegistry {
    /// Registers one live checker connection until the returned guard drops.
    pub fn register(self: &Arc<Self>, machine: String, platform: Platform) -> CheckerGuard {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.next_id = guard.next_id.wrapping_add(1);
        let id = guard.next_id;
        guard.active.insert(id, (machine.clone(), platform.clone()));
        guard.version = guard.version.wrapping_add(1);
        CheckerGuard {
            registry: Arc::clone(self),
            id,
            machine,
            platform,
        }
    }

    /// Monotonic counter bumped whenever a checker connects, disconnects, or a
    /// per-platform checkpoint advances, so the state feed notices promptly.
    pub fn version(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .version
    }

    /// Bumps the change counter (for example after a per-platform pass).
    pub fn bump_version(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard.version = guard.version.wrapping_add(1);
    }

    /// Unix milliseconds when this registry (and server) started.
    pub fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }

    /// Most recent disconnect timestamp (Unix ms) of any checker matching `target`.
    pub fn last_offline_for(&self, target: &Platform) -> Option<i64> {
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        guard
            .last_offline
            .iter()
            .filter(|(platform, _)| platform.matches(target))
            .map(|(_, at)| *at)
            .max()
    }

    /// Connected checkers whose reported platform satisfies `target`.
    pub fn matching(&self, target: &Platform) -> Vec<ConnectedChecker> {
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut matched: Vec<ConnectedChecker> = guard
            .active
            .values()
            .filter(|(_, platform)| platform.matches(target))
            .map(|(machine, platform)| ConnectedChecker {
                machine: machine.clone(),
                platform: platform.as_str().to_owned(),
            })
            .collect();
        matched.sort_by(|a, b| {
            a.machine
                .cmp(&b.machine)
                .then_with(|| a.platform.cmp(&b.platform))
        });
        matched
    }
}
