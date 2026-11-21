//! Tracks the latest spawned auth task so superseded login/signup attempts
//! are cancelled before their side effects land.
//!
//! Port of `components/auth/attempt_controller.rs`: same `begin` /
//! `matches` / `cancel` / `finish` semantics, but the state lives on the
//! owning screen's entity (fields instead of signals) and the task is a
//! gpui `Task` (drop = cancel).

use gpui::Task;

#[derive(Default)]
pub struct LatestAttemptController {
    counter: u64,
    active_attempt: Option<u64>,
    task: Option<Task<()>>,
}

impl LatestAttemptController {
    /// Cancels any in-flight attempt, bumps the counter, and returns the new
    /// attempt id for `matches` checks inside the spawned future.
    pub fn begin(&mut self) -> u64 {
        self.cancel();
        self.counter = self.counter.wrapping_add(1);
        self.active_attempt = Some(self.counter);
        self.counter
    }

    /// Stores the task for the active attempt. Drop of a previous task
    /// already cancelled it, so this is a plain replace.
    pub fn replace_task(&mut self, task: Task<()>) {
        self.task = Some(task);
    }

    pub fn cancel(&mut self) {
        // Dropping the gpui task cancels it.
        self.task = None;
        self.active_attempt = None;
    }

    pub fn reset(&mut self, cleanup: impl FnOnce()) {
        self.cancel();
        cleanup();
    }

    /// Clears bookkeeping when the attempt with `attempt_id` completed.
    pub fn finish(&mut self, attempt_id: u64) {
        if self.matches(attempt_id) {
            self.task = None;
            self.active_attempt = None;
        }
    }

    pub const fn is_busy(&self) -> bool {
        self.active_attempt.is_some()
    }

    pub fn matches(&self, attempt_id: u64) -> bool {
        self.active_attempt == Some(attempt_id)
    }
}
