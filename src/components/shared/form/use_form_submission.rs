//! Shared form-submission plumbing over [`FormFieldHandle`].
//!
//! Two pieces every hand-rolled form in this crate repeats:
//!
//! - [`use_form_input`] — one `oninput` closure that applies the change to a
//!   field handle and lets the caller react (clearing errors, resetting an
//!   attempt tracker) *before* the new value lands. It only ever writes the
//!   value into the signal; it never touches the DOM node, so IME composition
//!   keeps working exactly as it does with a hand-written handler.
//! - [`use_auto_submit`] — the effect shape of the reactive auto-submit
//!   forms: fire `submit` once per distinct input tuple while valid and idle,
//!   and never re-fire the same tuple (duplicate-attempt suppression).

use dioxus::prelude::*;

use super::FormFieldHandle;

/// Applies `next` to the field and calls `on_change` with it first.
///
/// The ordering matters at the call sites: `on_change` clears stale errors and
/// attempt trackers while it can still observe the *old* value, then the field
/// itself updates and validation/dirty state follow. Returns the new value so
/// a caller can keep chaining (e.g. a PIN filter that also edits the field).
pub fn apply_form_input(
    mut field: FormFieldHandle,
    next: String,
    mut on_change: impl FnMut(&str),
) -> String {
    on_change(&next);
    field.value.set(next.clone());
    field.is_dirty.set(true);
    next
}

/// One deduped auto-submit gate: remember the last input tuple that was
/// submitted and refuse to re-submit it.
///
/// Classic pattern across the auth forms: a `use_effect` observes validity,
/// builds the input tuple, compares it against `last_attempt`, and spawns the
/// attempt. Without the tracker the effect would re-fire after the attempt
/// fails and loop forever; with it, a retry is the caller *clearing* the
/// tracker so the same tuple may fire once more.
pub struct AutoSubmitGate<T> {
    last_attempt: Signal<Option<T>>,
}

// `Signal` is Copy; a derived impl would be conditional on `T: Copy`, and the
// attempt tuples aren't Copy — spell the impls out so the gate stays Copy.
impl<T> Clone for AutoSubmitGate<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for AutoSubmitGate<T> {}

impl<T: PartialEq + 'static> AutoSubmitGate<T> {
    pub fn use_gate() -> Self {
        Self {
            last_attempt: use_signal(|| None),
        }
    }

    /// True when `attempt` differs from the last submitted tuple; records it
    /// and returns `true` so the caller proceeds. A repeated tuple returns
    /// `false` and leaves state untouched.
    pub fn claim(mut self, attempt: T) -> bool {
        if self.last_attempt.read().as_ref() == Some(&attempt) {
            return false;
        }
        *self.last_attempt.write() = Some(attempt);
        true
    }

    /// Forgets the last attempt so the same tuple may fire again (retry
    /// button) — or, with a changed tuple, so the old entry stops shadowing.
    ///
    /// A caller may pass the tuple it is retiring: when the tracker still
    /// holds that tuple it is cleared, and when it holds a different (newer)
    /// one it is left alone.
    pub fn release(mut self, retiring: Option<&T>) {
        let matches_retiring = self
            .last_attempt
            .read()
            .as_ref()
            .zip(retiring)
            .is_some_and(|(held, retired)| held == retired);
        if matches_retiring || self.last_attempt.peek().is_none() {
            return;
        }
        *self.last_attempt.write() = None;
    }

    /// Unconditionally forgets the last attempt (form reset, superseded
    /// attempt, success that should allow re-running the same input).
    pub fn clear(mut self) {
        if self.last_attempt.peek().is_some() {
            *self.last_attempt.write() = None;
        }
    }

    /// The tuple currently recorded, if any.
    pub fn peek(&self) -> Option<T>
    where
        T: Clone,
    {
        self.last_attempt.read().as_ref().cloned()
    }
}
