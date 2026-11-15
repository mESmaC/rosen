//! Keep optimistic landing concurrent until a losing request needs a turn.
use std::sync::{Condvar, Mutex, PoisonError};

#[derive(Default)]
pub(super) struct Admission {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    optimistic: usize,
    next_ticket: u64,
    serving: u64,
}

/// Releasing on every exit, including errors and unwinding, keeps a failed
/// attempt from blocking all subsequent lands.
pub(super) struct Turn<'a> {
    admission: &'a Admission,
    exclusive: bool,
}

impl Admission {
    pub(super) fn optimistic(&self) -> Turn<'_> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Once a retry queues, new attempts must not prolong its wait.
        let mut state = self
            .changed
            .wait_while(state, |s| s.next_ticket != s.serving)
            .unwrap_or_else(PoisonError::into_inner);
        state.optimistic += 1;
        Turn {
            admission: self,
            exclusive: false,
        }
    }

    pub(super) fn exclusive(&self) -> Turn<'_> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        // Tickets prevent later retries from barging ahead on platforms
        // whose native reader/writer locks do not guarantee writer fairness.
        let _state = self
            .changed
            .wait_while(state, |s| s.optimistic != 0 || s.serving != ticket)
            .unwrap_or_else(PoisonError::into_inner);
        Turn {
            admission: self,
            exclusive: true,
        }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let mut state = self
            .admission
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.exclusive {
            state.serving = state.serving.wrapping_add(1);
        } else {
            state.optimistic -= 1;
        }
        self.admission.changed.notify_all();
    }
}
