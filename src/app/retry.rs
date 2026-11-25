//! Retry/backoff helpers for long-lived background loops.
//!
//! The core [`retry_until_ok`] loop + nudge-aware backoff sleep used by the
//! app-startup phases. Other retry loops in the codebase (identity pull
//! rounds, pairing publish verification, dead-letter retries) have
//! deliberately different schedules and stopping conditions — they are kept
//! as their own small loops rather than forced through knob-heavy
//! configuration.
//!
//! The loop stops when its classifier calls a failure permanent: retrying it
//! would hold the caller in a state it cannot leave, which is why
//! [`retry_until_ok`] reports the give-up decision instead of looping forever
//! (see [`is_permanent_stored_row_error`]).

use std::{fmt::Display, str::Utf8Error, string::FromUtf8Error, time::Duration};

use anyhow::Error;
use recapn::Error as RecapnError;
use tokio::select;

use dioxus::logger::tracing::warn;

use crate::{
    helpers::async_utils::timeout,
    storage::{
        background_errors::{ErrorSeverity, report_nonfatal_with_severity},
        schema_version::SchemaVersionError,
    },
    sync::{APP_ACTIVITY_CHANGED, NETWORK_CHANGED},
    user::MalformedUserRow,
};

const RETRY_FAST_ATTEMPTS: u32 = 3;
const RETRY_FAST_DELAY_MS: u64 = 50;
const RETRY_BASE_DELAY_MS: u64 = 100;
const RETRY_MAX_DELAY_MS: u64 = 5_000;
const RETRY_LOG_INTERVAL: u32 = 8;

/// Fast-window exponential backoff delay for `attempt` (1-based): the first
/// [`RETRY_FAST_ATTEMPTS`] attempts retry immediately, then
/// `base << exp`, capped at [`RETRY_MAX_DELAY_MS`].
fn retry_delay_ms(attempt: u32) -> u64 {
    if attempt <= RETRY_FAST_ATTEMPTS {
        return RETRY_FAST_DELAY_MS;
    }
    let exp = attempt.saturating_sub(RETRY_FAST_ATTEMPTS + 1).min(10);
    RETRY_BASE_DELAY_MS
        .saturating_mul(1_u64 << exp)
        .min(RETRY_MAX_DELAY_MS)
}

/// Sleeps the shared backoff schedule before one retry, logging throttled
/// warnings and surfacing persistent failures to the Activity panel under
/// `domain`. The sleep is nudge-aware: a network or app-lifecycle event cuts
/// it short so reconnection-driven retries start immediately.
async fn sleep_before_retry(
    domain: &'static str,
    severity: ErrorSeverity,
    attempt: u32,
    previous_delay_ms: &mut u64,
    failure_scope: &str,
    err: &impl Display,
) {
    let delay_ms = retry_delay_ms(attempt);
    let delay_changed = delay_ms != *previous_delay_ms;
    *previous_delay_ms = delay_ms;
    // `{err:#}` prints an `anyhow` chain whole; its plain `Display` shows only
    // the outermost context, which is the same context the caller wrapped the
    // failure in — the cause that matters would never reach the log.
    if delay_changed || attempt <= RETRY_FAST_ATTEMPTS || attempt.is_multiple_of(RETRY_LOG_INTERVAL)
    {
        warn!("{failure_scope} attempt {attempt} failed: {err:#}; retrying in {delay_ms}ms");
    }
    // Surface persistent failures to the Activity panel: once past the fast
    // window, report at first opportunity and then on the log cadence so a
    // stuck flow is never silent. Success clears the domain (see callers).
    if attempt > RETRY_FAST_ATTEMPTS
        && (attempt == RETRY_FAST_ATTEMPTS + 1 || attempt.is_multiple_of(RETRY_LOG_INTERVAL))
    {
        report_nonfatal_with_severity(
            domain,
            severity,
            "Retrying",
            format!("{failure_scope} attempt {attempt} failed: {err:#}"),
        );
    }
    let nudged = timeout(Duration::from_millis(delay_ms), async {
        select! {
            () = NETWORK_CHANGED.notified() => {}
            () = APP_ACTIVITY_CHANGED.notified() => {}
        }
    })
    .await
    .is_ok();
    if nudged && (attempt <= RETRY_FAST_ATTEMPTS || attempt.is_multiple_of(RETRY_LOG_INTERVAL)) {
        warn!("{failure_scope} retry nudged by network/lifecycle event (attempt {attempt})");
    }
}

/// True for a *stored row that will not decode*: a schema-version mismatch
/// ([`SchemaVersionError`]) or bytes that are not the packed Cap'n Proto
/// message / UTF-8 text the row claims to be.
///
/// The same bytes fail identically on every attempt, so a retry loop
/// classifying with this predicate reports once and stops. Conditions that
/// clear on their own — table I/O, lock contention, the local Veilid API still
/// starting — are deliberately not in this set and stay in the loop.
pub fn is_permanent_stored_row_error(error: &Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<SchemaVersionError>().is_some()
            || cause.downcast_ref::<RecapnError>().is_some()
            || cause.downcast_ref::<FromUtf8Error>().is_some()
            || cause.downcast_ref::<Utf8Error>().is_some()
            || cause.downcast_ref::<MalformedUserRow>().is_some()
    })
}

/// Classifier for a loop whose failures are all transient, so it never gives
/// up — e.g. waiting for the network to attach.
pub const fn never_permanent<E>(_: &E) -> bool {
    false
}

/// [`is_permanent_stored_row_error`], plus a storage refusal the *browser*
/// made.
///
/// `StorageProblem` is raised before Veilid's own config is built
/// (`platform::browser_storage`), because the reason disappears once Veilid
/// maps the `DOMException`. Only the user can clear it — leaving Private
/// Browsing, allowing site data, freeing space — so retrying cannot succeed and
/// the loop must hand the entry point back with the reason attached instead of
/// holding the startup splash forever.
///
/// Off wasm the variant does not exist, so this is exactly
/// [`is_permanent_stored_row_error`].
pub fn is_permanent_local_error(error: &Error) -> bool {
    if is_permanent_stored_row_error(error) {
        return true;
    }
    #[cfg(target_arch = "wasm32")]
    {
        // Walked over the whole chain, like the predicate above: the probe's
        // error carries at least one `.context(..)` by the time it reaches the
        // loop, and a plain `downcast_ref` on the outermost error would then
        // silently stop matching — turning the explained splash back into the
        // retry-forever one this exists to prevent.
        error.chain().any(|cause| {
            cause
                .downcast_ref::<crate::platform::browser_storage::StorageProblem>()
                .is_some()
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

/// Retries `op` until it succeeds or `is_permanent` says retrying cannot help,
/// using the shared backoff schedule ([`sleep_before_retry`]) and reporting
/// persistent failures under `domain`.
///
/// Returns `Some(value)` on success, and `None` when it gave up on a permanent
/// failure: that error is reported once (`Stopped retrying`) and the caller
/// decides what the degraded state is. A loop that must never stop passes
/// [`never_permanent`].
pub async fn retry_until_ok<T, E: Display>(
    domain: &'static str,
    severity: ErrorSeverity,
    failure_scope: &'static str,
    is_permanent: impl Fn(&E) -> bool,
    mut op: impl AsyncFnMut() -> Result<T, E>,
) -> Option<T> {
    let mut attempt = 0_u32;
    let mut previous_delay_ms = 0_u64;
    loop {
        attempt = attempt.saturating_add(1);
        match op().await {
            Ok(value) => return Some(value),
            Err(err) => {
                // A failure that cannot clear must not be retried: the caller's
                // permanent state (the startup splash, the router's hydration
                // gate) is reached by *returning*, not by looping. Report it
                // once at full chain and hand the decision back.
                if is_permanent(&err) {
                    report_nonfatal_with_severity(
                        domain,
                        severity,
                        "Stopped retrying",
                        format!("{failure_scope} cannot succeed on retry: {err:#}"),
                    );
                    return None;
                }
                sleep_before_retry(
                    domain,
                    severity,
                    attempt,
                    &mut previous_delay_ms,
                    failure_scope,
                    &err,
                )
                .await;
            }
        }
    }
}
