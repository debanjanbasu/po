//! Background error reporting for detached async tasks.
//!
//! Unlike component-local futures, detached tasks are not caught by
//! `ErrorBoundary`. This module provides a minimal, user-visible error surface
//! while retaining detailed logs for diagnostics.

use std::{
    fmt::Display,
    future::Future,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

use anyhow::Result;
use dioxus::{logger::tracing::warn, prelude::*};

use crate::{config, helpers::spawn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSeverity {
    /// Log only; do not add this error to `BACKGROUND_ERRORS`.
    ///
    /// Constructed on web only: the browser-storage probe is the one report
    /// that must stay out of the user-visible surface. Elsewhere every report
    /// is `Visible`.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Silent,
    /// Add a user-visible entry to `BACKGROUND_ERRORS`.
    Visible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundError {
    pub id: u64,
    pub domain: &'static str,
    pub message: String,
    pub op_id: Option<u64>,
}

pub static BACKGROUND_ERRORS: GlobalSignal<Vec<BackgroundError>> = Signal::global(Vec::new);

static NEXT_BACKGROUND_ERROR_ID: AtomicU64 = AtomicU64::new(1);

fn new_background_error(domain: &'static str, message: String) -> BackgroundError {
    BackgroundError {
        id: NEXT_BACKGROUND_ERROR_ID.fetch_add(1, Relaxed),
        domain,
        message,
        op_id: None,
    }
}

fn truncate_errors(errors: &mut Vec<BackgroundError>) {
    let excess = errors
        .len()
        .saturating_sub(config::storage::MAX_BACKGROUND_ERRORS);
    errors.drain(..excess);
}

pub fn push_background_error(domain: &'static str, message: impl Into<String>) {
    BACKGROUND_ERRORS.with_mut(|errors| {
        errors.push(new_background_error(domain, message.into()));
        truncate_errors(errors);
    });
}

/// Format an error for the log and the Activity modal.
///
/// `anyhow::Error`'s `Display` prints only the outermost context, so a
/// chained error showed just "failed to open DHT record" and hid the cause
/// that actually mattered ("no node answered the open"). The alternate
/// `{:#}` form prints the whole chain, and for every other error type it
/// is identical to `Display`.
fn error_chain(error: &impl Display) -> String {
    format!("{error:#}")
}

pub fn report_nonfatal(domain: &'static str, context: &'static str, error: impl Display) {
    let error = error_chain(&error);
    warn!("{context}: {error}");
    push_background_error(domain, format!("{context}: {error}"));
}

/// Concise macro for non-fatal error reporting. Replaces the 3-line
/// `if let Err(e) = expr { report_nonfatal(...); }` pattern.
///
/// Usage:
/// ```ignore
/// nonfatal!("domain", "context message", some_fallible_expr);
/// ```
#[macro_export]
macro_rules! nonfatal {
    ($domain:expr, $msg:expr, $expr:expr) => {
        if let Err(e) = $expr {
            $crate::storage::background_errors::report_nonfatal($domain, $msg, e);
        }
    };
}

pub fn report_nonfatal_with_severity(
    domain: &'static str,
    severity: ErrorSeverity,
    context: &'static str,
    error: impl Display,
) {
    let error = error_chain(&error);
    warn!("{context}: {error}");
    // `Silent` stays out of the persisted error surface; the log line above
    // still records it.
    if !matches!(severity, ErrorSeverity::Silent) {
        push_background_error(domain, format!("{context}: {error}"));
    }
}

/// Reports an error tied to a specific queue operation. Tagged with the op's
/// id so it auto-clears via [`clear_op_errors`] once a later retry of the same
/// op succeeds or the op is retried/dismissed from the dead-letter queue.
/// Replaces any previous entry for the same op — one live notice per op.
pub fn push_op_error(domain: &'static str, severity: ErrorSeverity, message: String, op_id: u64) {
    if matches!(severity, ErrorSeverity::Silent) {
        return;
    }
    BACKGROUND_ERRORS.with_mut(|errors| {
        errors.retain(|e| e.op_id != Some(op_id));
        let mut error = new_background_error(domain, message);
        error.op_id = Some(op_id);
        errors.push(error);
        truncate_errors(errors);
    });
}

/// Removes error entries tagged with `op_id` so stale "sync gave up" notices
/// don't outlive their resolution (retry succeeded, or user retried/dismissed
/// the dead-lettered op). Peeks first: the common success path must not write
/// the signal (and re-render subscribers) when nothing matches.
pub fn clear_op_errors(op_id: u64) {
    if !BACKGROUND_ERRORS
        .peek()
        .iter()
        .any(|e| e.op_id == Some(op_id))
    {
        return;
    }
    BACKGROUND_ERRORS.with_mut(|errors| errors.retain(|e| e.op_id != Some(op_id)));
}

/// Removes every error entry for `domain` (e.g. a retry loop that eventually
/// succeeded). Peeks first: success paths must not write the signal (and
/// re-render subscribers) when nothing matches.
pub fn clear_errors_for_domain(domain: &str) {
    if !BACKGROUND_ERRORS.peek().iter().any(|e| e.domain == domain) {
        return;
    }
    BACKGROUND_ERRORS.with_mut(|errors| errors.retain(|e| e.domain != domain));
}

pub fn spawn_report(
    domain: &'static str,
    context: &'static str,
    task: impl Future<Output = Result<()>> + 'static,
) {
    spawn(async move {
        if let Err(e) = task.await {
            report_nonfatal(domain, context, e);
        }
    });
}

/// Dismisses by stable [`BackgroundError::id`], not by list position: the
/// list drops its oldest entries when it truncates, so an index captured at
/// render time can address a different error once the list has shifted.
pub fn dismiss_background_error(id: u64) {
    BACKGROUND_ERRORS.with_mut(|errors| errors.retain(|error| error.id != id));
}

pub fn clear_background_errors() {
    BACKGROUND_ERRORS.with_mut(Vec::clear);
}
