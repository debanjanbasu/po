use std::fmt::Display;

use crate::storage::background_errors::report_nonfatal;

#[cfg(target_os = "android")]
pub mod android_plugin;
#[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
pub mod apple_ffi;
// The web build's only local-store dependency is a browser write (see the
// module). Nothing else has a browser to probe.
#[cfg(target_arch = "wasm32")]
pub mod browser_storage;
pub mod deep_link;
pub mod device_id;
pub mod lifecycle;
// Android and the Apple targets are the only consumers of the JNI/extern-C
// bridge: the harness seams, deep-link intake and the lifecycle notifies. Web
// has no native process to notify.
#[cfg(all(
    any(target_os = "android", target_os = "ios", target_os = "macos"),
    feature = "mobile"
))]
pub mod native_bridge;
pub mod qr;
#[cfg(feature = "mobile")]
#[cfg(feature = "mobile")]
pub mod qr_scanner;
#[cfg(feature = "mobile")]
pub mod share;
pub mod share_urls;
// The native window is this app's own surface: it draws its own chrome on
// every desktop and hides every system bar on every phone, so the only
// platforms with nothing to configure here are web (the browser owns its
// chrome) and Android (its activity does the hiding, see
// `native/android/MainActivity.kt`).
#[cfg(any(
    all(
        feature = "desktop",
        any(target_os = "macos", target_os = "windows", target_os = "linux")
    ),
    all(feature = "mobile", target_os = "ios")
))]
#[cfg(target_os = "windows")]
use std::future::Future;

#[cfg(target_os = "windows")]
use tokio::{
    runtime::{Builder, Handle},
    task::block_in_place,
};

/// Runs a WinRT future from synchronous Windows bridge code.
///
/// Uses the current Tokio runtime when available, otherwise creates a
/// temporary current-thread runtime.
#[cfg(target_os = "windows")]
pub fn block_on_winrt<F>(future: F) -> Option<F::Output>
where
    F: Future,
{
    if let Ok(handle) = Handle::try_current() {
        return Some(block_in_place(|| handle.block_on(future)));
    }
    let runtime = Builder::new_current_thread().enable_all().build().ok()?;
    Some(runtime.block_on(future))
}

/// Runs `work` inside an Objective-C autorelease pool on Apple targets.
///
/// The `manganis` Swift bridge converts every `String` argument with
/// `-[NSString stringWithUTF8String:]` and reads the result with `-UTF8String`
/// (`packages/manganis/manganis-macro/src/ffi.rs`, `generate_ios_function`).
/// Both return autoreleased objects and the generated body never creates a
/// pool, so a call made from a thread without one leaks its argument and its
/// result string. Every Swift plugin call in this crate is made from a
/// `spawn_blocking` thread, which is a bare pthread with no pool, so the pool is
/// taken here, around the native call.
///
/// This belongs upstream, not here: wrapping the body `generate_ios_function`
/// emits in `objc2::rc::autoreleasepool` fixes every call site at once
/// and cannot be forgotten by the next plugin. Upstream candidate is the fork
/// `debanjanbasu/dioxus` at the revision `Cargo.lock` pins (manganis
/// 0.8.0-alpha.3); until that lands, keep new native calls inside this wrapper.
///
/// A no-op off Apple — Android's bridge passes `String` through JNI, which owns
/// its own local references.
#[cfg(all(
    any(target_os = "ios", target_os = "macos", target_os = "android"),
    feature = "mobile"
))]
pub fn with_autorelease_pool<R>(work: impl FnOnce() -> R) -> R {
    #[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
    {
        objc2::rc::autoreleasepool(|_| work())
    }
    #[cfg(target_os = "android")]
    {
        work()
    }
}

/// Reports `context` on failure and yields the native result, or `None` to fall back.
pub fn native_call<T, E: Display>(
    result: Result<T, E>,
    domain: &'static str,
    context: &'static str,
) -> Option<T> {
    result
        .map_err(|error| report_nonfatal(domain, context, error))
        .ok()
}
