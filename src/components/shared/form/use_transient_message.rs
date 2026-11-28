//! `use_transient_message`: a timed transient message signal with a
//! stale-generation guard.
//!
//! Each `set` bumps an internal generation before starting the dismiss timer;
//! a timer only clears the message if it is still the newest one, so a stale
//! timer can never cut a newer message's display time short.

use crate::helpers::{async_utils::sleep_ms, spawn};
use dioxus::prelude::*;

/// Handle for arming/clearing a [`use_transient_message`] signal.
#[derive(Clone, Copy)]
pub struct TransientSetter {
    message: Signal<Option<String>>,
    generation: Signal<u64>,
    duration_ms: u64,
}

impl TransientSetter {
    /// Show `text` for the configured duration, restarting any pending timer.
    pub fn set(&mut self, text: impl Into<String>) {
        self.message.set(Some(text.into()));
        let generation = self.bump_generation();
        // Signals are `Copy`; bind them locally so the task owns values
        // instead of borrowing `self` across the `'static` spawn.
        let mut message = self.message;
        let generation_signal = self.generation;
        let duration_ms = self.duration_ms;
        spawn(async move {
            sleep_ms(duration_ms).await;
            if *generation_signal.peek() == generation {
                message.set(None);
            }
        });
    }

    /// Hide the message immediately and invalidate any pending timer.
    pub fn clear(&mut self) {
        self.bump_generation();
        self.message.set(None);
    }

    fn bump_generation(&mut self) -> u64 {
        let generation = self.generation.peek().wrapping_add(1);
        self.generation.set(generation);
        generation
    }
}

/// Timed transient message state: `(message, setter)` with a per-set
/// generation guard against stale dismissal timers.
#[must_use]
pub fn use_transient_message(duration_ms: u64) -> (Signal<Option<String>>, TransientSetter) {
    let message = use_signal(|| None::<String>);
    let generation = use_signal(|| 0_u64);
    (
        message,
        TransientSetter {
            message,
            generation,
            duration_ms,
        },
    )
}
