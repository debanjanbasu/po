//! Platform-agnostic async utilities (sleep, timers, spawning).

#[cfg(not(target_arch = "wasm32"))]
use anyhow::Context;
use anyhow::Result;
#[cfg(target_arch = "wasm32")]
use js_sys::{Date, Promise};
#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    future::Future,
    io::{Error as IoError, ErrorKind},
    time::Duration,
};

use crate::config;

pub use dioxus::prelude::spawn;
#[cfg(target_arch = "wasm32")]
use tokio::select;
#[cfg(not(target_arch = "wasm32"))]
use tokio::{
    task::spawn_blocking as tokio_spawn_blocking,
    time::{sleep, timeout as tokio_timeout},
};

#[cfg(target_arch = "wasm32")]
use {
    wasm_bindgen::{JsValue, UnwrapThrowExt},
    wasm_bindgen_futures::JsFuture,
    web_sys::window,
};

/// Platform-agnostic timeout wrapper.
pub async fn timeout<F, T>(duration: Duration, future: F) -> Result<T, IoError>
where
    F: Future<Output = T>,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio_timeout(duration, future)
            .await
            .map_err(|_| IoError::new(ErrorKind::TimedOut, "operation timed out"))
    }

    #[cfg(target_arch = "wasm32")]
    {
        let delay = js_sleep(duration.as_millis().try_into().unwrap_or(u32::MAX));
        select! {
            biased;
            result = future => Ok(result),
            result = delay => {
                result?;
                Err(IoError::new(ErrorKind::TimedOut, "operation timed out"))
            }
        }
    }
}

/// Creates an async delay via `setTimeout` + `Promise` on WASM.
#[cfg(target_arch = "wasm32")]
async fn js_sleep(ms: u32) -> Result<(), IoError> {
    let promise = Promise::new(&mut |resolve, reject| {
        let Some(window) = window() else {
            reject
                .call1(&JsValue::NULL, &JsValue::from_str("no global window"))
                .unwrap_throw();
            return;
        };
        if let Err(error) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            &resolve,
            i32::try_from(ms).unwrap_or(i32::MAX),
        ) {
            reject.call1(&JsValue::NULL, &error).unwrap_throw();
        }
    });
    JsFuture::from(promise)
        .await
        .map(|_| ())
        .map_err(|error| IoError::other(error.as_string().unwrap_or_else(|| format!("{error:?}"))))
}

/// Platform-agnostic async sleep function.
pub async fn sleep_ms(ms: u64) {
    #[cfg(target_arch = "wasm32")]
    let _ = js_sleep(ms.try_into().unwrap_or(u32::MAX)).await;

    #[cfg(not(target_arch = "wasm32"))]
    sleep(Duration::from_millis(ms)).await;
}

/// Runs `op` once and, if it fails with a transient network error, waits
/// [`config::nudge::RETRY_DELAY_MS`] and runs it one more time.
///
/// Exactly one retry, deliberately: every caller is a DHT read whose stall is
/// already covered by the durable paths (watches, nudges, the periodic poll),
/// so a longer budget would delay the caller without changing convergence.
/// `is_transient` classifies the error — `store::is_transient_network_error`
/// is the usual choice.
pub async fn retry_once_on_transient<T, E, F, Fut>(
    is_transient: impl Fn(&E) -> bool,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    match op().await {
        Err(error) if is_transient(&error) => {
            sleep_ms(config::nudge::RETRY_DELAY_MS).await;
            op().await
        }
        result => result,
    }
}

/// Platform-agnostic milliseconds since Unix epoch.
/// `js_sys::Date::now()` is f64 milliseconds since the epoch; JS has no
/// integer epoch API, so the f64→u64 conversion is the conversion itself.
/// The date is non-negative by contract, and the `expect` is self-verifying:
/// if this cast is ever removed, the lint fires.
#[cfg(target_arch = "wasm32")]
fn wasm_now_millis() -> u64 {
    u64::try_from(Duration::from_secs_f64((Date::now() / 1_000.0).max(0.0)).as_millis())
        .unwrap_or(u64::MAX)
}

pub fn now_epoch_millis() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        wasm_now_millis()
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // No truncating/sign casts: as_secs() is u64, subsec_millis() is u32
        // (0..=999), so the sum equals as_millis() exactly.
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        duration.as_secs() * 1_000 + u64::from(duration.subsec_millis())
    }
}

/// Platform-agnostic seconds since Unix epoch.
pub fn now_epoch_secs() -> u64 {
    now_epoch_millis() / 1000
}

/// Spawns a CPU-intensive task on a background thread (native) or runs it immediately (WASM).
pub async fn spawn_blocking<F, R>(f: F) -> Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio_spawn_blocking(f)
            .await
            .context("blocking task panicked or was cancelled")
    }
    #[cfg(target_arch = "wasm32")]
    {
        Ok(f())
    }
}
