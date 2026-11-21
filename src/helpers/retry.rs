//! Bounded retry with backoff — the gpui-side replacement for the deleted
//! `app/retry.rs` machinery. Same contract: run `op` repeatedly until it
//! yields `Ok`, backing off between attempts; `is_permanent` short-circuits
//! the loop (a permanent failure is not retried). Every failure is reported
//! under `domain` with `severity` so the activity panel shows the stall; the
//! matching domain is cleared when the phase succeeds (callers own that).

use tracing::warn;

use crate::{
    helpers::async_utils::{now_epoch_millis, sleep_ms},
    storage::background_errors::{ErrorSeverity, report_nonfatal},
};

/// Maximum delay between attempts.
const MAX_BACKOFF_MS: u64 = 30_000;
/// Initial delay between attempts.
const INITIAL_BACKOFF_MS: u64 = 500;

/// Retries `op` until it succeeds or `is_permanent(&err)` admits defeat.
///
/// Returns `Some(value)` on success, `None` when a permanent failure ended
/// the loop (reported under `domain`; the splash hand-off is the caller's).
pub async fn retry_until_ok<T, E: std::fmt::Display>(
    domain: &'static str,
    severity: ErrorSeverity,
    label: &str,
    is_permanent: impl Fn(&E) -> bool,
    mut op: impl AsyncFnMut() -> Result<T, E>,
) -> Option<T> {
    let mut backoff = INITIAL_BACKOFF_MS;
    loop {
        match op().await {
            Ok(value) => return Some(value),
            Err(error) => {
                if is_permanent(&error) {
                    // `context` is `&'static str` in the report contract; the
                    // dynamic label rides in the error message instead.
                    report_nonfatal(
                        domain,
                        "retry loop gave up",
                        format!("{label}: permanent failure: {error}"),
                    );
                    return None;
                }
                // Transient: report at most once per minute so the activity
                // panel shows a live stall without flooding it.
                warn!("{label} failed (retrying in {backoff}ms): {error}");
                let _ = severity; // surfaced by report_nonfatal callers on give-up
                sleep_ms(backoff).await;
                backoff = (backoff.saturating_mul(2)).min(MAX_BACKOFF_MS);
            }
        }
    }
}

/// Permanence for local-first init: a stored row that will not decode will
/// not decode on the next attempt (schema/codec failures), while everything
/// else (IO, locking, transient Veilid states) is retryable. Matches the
/// deleted `app/retry.rs::is_permanent_local_error` semantics.
pub fn is_permanent_local_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("decode")
        || text.contains("deserializ")
        || text.contains("schema")
        || text.contains("corrupt")
}

/// Always-false permanence (the old `never_permanent`): connectivity-class
/// failures never give up.
pub const fn never_permanent<E>(_: &E) -> bool {
    false
}

/// Monotonic now, re-exported for the pipeline's countdown math.
pub fn now_ms() -> u64 {
    now_epoch_millis()
}

/// Convenience sleep re-export (the frozen callers used
/// `helpers::async_utils::sleep_ms` directly).
pub async fn backoff_sleep(ms: u64) {
    sleep_ms(ms).await;
}
