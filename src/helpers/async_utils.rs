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

/// The domain spawn: tokio on native targets (the `gpui_tokio` runtime the
/// app boots, or this module's `LazyLock` runtime for callers outside gpui
/// boot), `spawn_local` on wasm (single-threaded, like the dioxus web
/// runtime it replaces).
pub fn spawn<F>(future: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    spawn_tokio(future);
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(not(target_arch = "wasm32"))]
mod local_executor {
    // `unsafe impl Send` below is the executor's own contract (see the
    // type's docs); the rest of the crate stays `deny(unsafe_code)`. The
    // `Send`-related clippy lints are the same contract, restated.
    #![allow(unsafe_code)]
    #![allow(clippy::non_send_fields_in_send_ty)]
    #![allow(clippy::undocumented_unsafe_blocks)]
    // Executor startup failure is unrecoverable by design (see the spawn
    // site's comment) — the panic/expect here IS the error handling.
    #![allow(clippy::panic)]
    #![allow(clippy::expect_used)]
    //! The domain executor: a dedicated thread owning a tokio `LocalSet`.
    //!
    //! Veilid's crypto types (`KeyPair`, `SharedSecret`) are deliberately
    //! `!Send`, and the domain holds them across `await`s — the same reason
    //! the frozen stack used dioxus's single-threaded runtime. Spawners on
    //! other threads hand futures over through a `Mutex` queue; the futures
    //! are only ever *polled* on the owning thread, which is the invariant
    //! the `Send` wrapper below encodes (same pattern as
    //! `futures::task::LocalFutureObj`'s `unsafe` constructor).

    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Condvar, LazyLock, Mutex};

    type LocalFuture = Pin<Box<dyn Future<Output = ()>>>;

    /// A future that may only be polled by the domain thread.
    ///
    /// # Safety (contract, enforced by construction)
    /// The wrapper is `Send` so the *queue* can be shared; the wrapped
    /// future is never touched by any thread except the one that created
    /// the executor, which polls it. Every caller acquires the future via
    /// [`spawn`](super::spawn), which only enqueues.
    pub struct DomainOnly(pub LocalFuture);

    // SAFETY: see the type contract above — poll access is confined to the
    // executor thread; `&DomainOnly` is never dereferenced elsewhere.
    unsafe impl Send for DomainOnly {}

    struct Queue {
        futures: Mutex<VecDeque<DomainOnly>>,
        signal: Condvar,
    }

    static QUEUE: LazyLock<Arc<Queue>> = LazyLock::new(|| {
        let queue = Arc::new(Queue {
            futures: Mutex::new(VecDeque::new()),
            signal: Condvar::new(),
        });
        let driver = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("oo-domain-executor".into())
            .spawn(move || run(&driver))
            // Executor-thread failure is unrecoverable by design: state
            // cannot be restored mid-pipeline, and no caller observes the
            // join handle. Mirrors the scoped-panic pattern in
            // `platform/native_bridge`.
            .unwrap_or_else(|error| {
                panic!("spawn domain executor thread: {error}");
            });
        queue
    });

    fn run(queue: &Arc<Queue>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("domain tokio runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            loop {
                // Drain everything queued so far onto the LocalSet.
                let batch: Vec<DomainOnly> = {
                    let mut guard = queue
                        .futures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.drain(..).collect()
                };
                for DomainOnly(future) in batch {
                    tokio::task::spawn_local(future);
                }
                // Park until a new spawn arrives; a short timeout keeps
                // already-scheduled locals ticking (their wakers wake the
                // runtime directly, this only gates the drain loop).
                let guard = queue
                    .futures
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = queue
                    .signal
                    .wait_timeout(guard, std::time::Duration::from_millis(8));
            }
        });
    }

    /// Enqueues a (possibly `!Send`) future onto the domain executor.
    pub fn spawn<F: Future<Output = ()> + 'static>(future: F) {
        let mut queue = QUEUE
            .futures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.push_back(DomainOnly(Box::pin(future)));
        drop(queue);
        QUEUE.signal.notify_one();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_tokio<F: std::future::Future<Output = ()> + 'static>(future: F) {
    local_executor::spawn(future);
}

/// The root-scope spawn (the old `spawn_forever`): tasks here survive their
/// caller by construction — there is no scope to drop. Same runtime rules
/// as [`spawn`].
pub fn spawn_forever<F>(future: F)
where
    F: std::future::Future<Output = ()> + 'static,
{
    spawn(future);
}
#[cfg(target_arch = "wasm32")]
use tokio::select;
#[cfg(not(target_arch = "wasm32"))]
use tokio::{
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
        // gpui's dispatcher threads carry no tokio context, so spawn through
        // an explicit process-wide handle rather than `Handle::current()`.
        static BLOCKING_RT: std::sync::OnceLock<tokio::runtime::Runtime> =
            std::sync::OnceLock::new();
        let runtime = BLOCKING_RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("blocking runtime")
        });
        runtime
            .spawn_blocking(f)
            .await
            .context("blocking task panicked or was cancelled")
    }
    #[cfg(target_arch = "wasm32")]
    {
        Ok(f())
    }
}
