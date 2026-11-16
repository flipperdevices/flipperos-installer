// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Flipper FZCO

//! The web frontend's one subscription to the controller, shared by every
//! connection.
//!
//! A subscriber runs on whichever thread changed the state, while the
//! controller holds its subscriber list, so it must neither block nor call
//! back in. This one only stores the snapshot and wakes the waiting
//! connections, which then read it at their own pace.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::core::model::AppState;
use crate::core::Controller;

pub struct Hub {
    state: Mutex<Arc<AppState>>,
    changed: Condvar,
}

impl Hub {
    pub fn new(ctrl: &Controller) -> Arc<Hub> {
        let hub = Arc::new(Hub {
            state: Mutex::new(Arc::new(ctrl.snapshot())),
            changed: Condvar::new(),
        });
        let h = Arc::clone(&hub);
        ctrl.subscribe(move |s| {
            *h.state.lock().unwrap() = Arc::new(s);
            h.changed.notify_all();
        });
        hub
    }

    pub fn snapshot(&self) -> Arc<AppState> {
        Arc::clone(&self.state.lock().unwrap())
    }

    /// Wait until the log grows past `seq` or the phase moves off `phase`, or
    /// until `timeout`, and return the state as it then is.
    pub fn wait(&self, seq: u64, phase_busy: bool, timeout: Duration) -> Arc<AppState> {
        let guard = self.state.lock().unwrap();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |s| {
                s.log_seq == seq && s.phase.is_busy() == phase_busy
            })
            .unwrap();
        Arc::clone(&guard)
    }
}
