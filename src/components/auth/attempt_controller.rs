//! Tracks the latest spawned auth task so superseded login/signup attempts are
//! cancelled before their side effects land (`use_latest_attempt_controller`).

use dioxus::{core::Task, prelude::*};

#[derive(Clone, Copy)]
pub struct LatestAttemptController {
    counter: Signal<u64>,
    active_attempt: Signal<Option<u64>>,
    task: Signal<Option<Task>>,
}

pub fn use_latest_attempt_controller() -> LatestAttemptController {
    LatestAttemptController {
        counter: use_signal(|| 0_u64),
        active_attempt: use_signal(|| None::<u64>),
        task: use_signal(|| None::<Task>),
    }
}

impl LatestAttemptController {
    pub fn begin(mut self) -> u64 {
        self.cancel();
        let mut attempt_id = 0;
        self.counter.with_mut(|current| {
            *current = current.wrapping_add(1);
            attempt_id = *current;
        });
        self.active_attempt.set(Some(attempt_id));
        attempt_id
    }

    pub fn replace_task(mut self, task: Task) {
        self.task.set(Some(task));
    }

    pub fn cancel(mut self) {
        if let Some(active_task) = (self.task)() {
            active_task.cancel();
            self.task.set(None);
        }
        if self.active_attempt.peek().is_some() {
            self.active_attempt.set(None);
        }
    }

    pub fn reset(self, cleanup: impl FnOnce()) {
        self.cancel();
        cleanup();
    }

    pub fn finish(mut self, attempt_id: u64) {
        if self.matches(attempt_id) {
            self.task.set(None);
            self.active_attempt.set(None);
        }
    }

    pub fn is_busy(self) -> bool {
        self.active_attempt.read().is_some()
    }

    pub fn matches(self, attempt_id: u64) -> bool {
        (self.active_attempt)() == Some(attempt_id)
    }
}
