//! Deep link URL interception and dispatch.
//!
//! On iOS/macOS and Android, native FFI surfaces the URL that opened the app
//! (either `po://...` or `https://po-ai.pages.dev/...`). Startup captures at
//! most one pending launch URL, route-level consumers may peek at the pending
//! route target, and only the designated screen owner may consume the typed
//! action payload. Apple launch handlers are installed before the event
//! loop runs so cold URLs survive until the reactive app state is ready.

use std::sync::Arc;

use crate::config;
use crate::platform::share_urls::{self, ShareUrl};
#[cfg(any(target_os = "ios", target_os = "macos"))]
use crate::{
    platform::{
        apple_ffi::{DeepLinkPlugin, pending_url, prepare_for_launch},
        native_call,
    },
    storage::background_errors::report_nonfatal,
};
use dioxus::{logger::tracing::warn, prelude::*};

/// Returns and consumes the URL that opened the native app.
#[cfg(target_os = "android")]
fn take_pending_url() -> String {
    super::native_bridge::take_pending_deep_link_url()
}

/// Returns and consumes the URL that opened the native app.
#[cfg(any(target_os = "ios", target_os = "macos"))]
fn take_pending_url() -> String {
    let plugin = match DeepLinkPlugin::new() {
        Ok(plugin) => plugin,
        Err(error) => {
            report_nonfatal("platform.deep_link", "Deep-link plugin unavailable", error);
            return String::new();
        }
    };

    let url = pending_url(&plugin);
    native_call(url, "platform.deep_link", "Cannot read the launch URL").unwrap_or_default()
}

/// Web and non-mobile desktop targets have no native launch URL source.
#[cfg(not(any(target_os = "ios", target_os = "macos", target_os = "android")))]
const fn take_pending_url() -> String {
    String::new()
}

/// Installs Apple URL and lifecycle handlers before the event loop runs.
/// On iOS, construct Tao's event loop first so its app delegate exists;
/// the plugin then hooks the scene configuration before `UIKit` connects it.
///
/// Must not touch signals or `report_nonfatal`: it runs before `launch()`.
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub fn bootstrap_apple_url_handler() {
    match DeepLinkPlugin::new()
        .and_then(|plugin| prepare_for_launch(&plugin))
        .map_err(String::from)
    {
        Ok(result) if result == "ok" => {}
        Ok(error) | Err(error) => {
            warn!("Apple launch integration unavailable: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeepLinkOwner {
    NavLayout,
    Pairing,
}

/// Parsed deep-link action retained until the owning screen consumes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeepLinkAction {
    AddContact {
        username: Arc<str>,
        dht_key: Arc<str>,
    },
    Pair {
        payload: Arc<str>,
    },
    /// Harness-only send: [`config::deep_link::SEND_SCHEME`] carries a message
    /// into `contact`'s conversation. The channel a surface with no typing
    /// channel sends through; refused unless [`config::e2e::enabled`] is on.
    SendMessage {
        contact: Arc<str>,
        text: Arc<str>,
    },
}

impl DeepLinkAction {
    const fn owner(&self) -> DeepLinkOwner {
        match self {
            Self::AddContact { .. } | Self::SendMessage { .. } => DeepLinkOwner::NavLayout,
            Self::Pair { .. } => DeepLinkOwner::Pairing,
        }
    }
}

static PENDING_ACTION: GlobalSignal<Option<DeepLinkAction>> = Signal::global(|| None);

/// Reads `take_pending_url()` once, parses it into a [`DeepLinkAction`], and stores
/// the result. Call this during startup hydration before any component can
/// consume the URL.
///
/// Returns `true` when a new typed deep-link action was queued.
pub fn consume_pending_action() -> bool {
    let url = take_pending_url();
    if url.is_empty() {
        return false;
    }
    let action = parse_action(&url);
    // The only observable that a native launch URL actually reached the app.
    // `S1` relaunches a native joiner with the env seams already pre-filling
    // the pairing form, so a completed pairing does not by itself prove the
    // `po://` link was delivered — this marker is what separates the two.
    // Never log the URL: a pairing link embeds the PIN.
    config::e2e::event!("deep_link", accepted = action.is_some());
    let Some(action) = action else {
        warn!("Ignored unsupported deep link");
        return false;
    };
    *PENDING_ACTION.write() = Some(action);
    true
}

/// Refreshes pending deep-link action from the platform only when no parsed
/// action is currently queued.
///
/// Returns `true` when a new typed deep-link action was queued.
pub fn refresh_pending_action_if_empty() -> bool {
    if PENDING_ACTION.peek().is_some() {
        return false;
    }
    consume_pending_action()
}

/// Returns whether any parsed deep-link action is still queued for its owning
/// screen.
pub fn has_pending_action() -> bool {
    PENDING_ACTION.read().is_some()
}

/// Returns whether a pairing deep link is pending consumption.
pub fn has_pending_pairing() -> bool {
    matches!(*PENDING_ACTION.read(), Some(DeepLinkAction::Pair { .. }))
}

/// Consumes the queued deep-link action only if `owner` is its designated
/// screen owner. Mismatched owners leave the action pending.
pub fn take_pending_action(owner: DeepLinkOwner) -> Option<DeepLinkAction> {
    if PENDING_ACTION
        .peek()
        .as_ref()
        .is_some_and(|action| action.owner() == owner)
    {
        PENDING_ACTION.write().take()
    } else {
        None
    }
}

/// Dioxus hook that claims the queued deep link for exactly one screen owner.
///
/// The native event pump updates reactive pending state. Claiming or finishing
/// an action wakes consumers directly, without a separate refresh counter.
pub fn use_claimed_action(owner: DeepLinkOwner) -> Signal<Option<DeepLinkAction>> {
    let mut claimed_action = use_signal(|| None::<DeepLinkAction>);
    use_effect(move || {
        let pending = has_pending_action();
        if claimed_action.read().is_some() {
            return;
        }
        if !pending {
            refresh_pending_action_if_empty();
        }
        if let Some(action) = take_pending_action(owner) {
            claimed_action.set(Some(action));
        }
    });
    claimed_action
}

/// Parses the harness-only send action:
/// `po://send?to=<username>&text=<text>`.
///
/// Refused unless [`config::e2e::enabled`] is on, so a distributed build drops
/// the link like any other unsupported URL.
///
/// shortcut: the query values are read verbatim (no percent-decoding), so a
/// `text` containing `&` or `#` truncates; upgrade if the harness needs them.
fn parse_send_action(url: &str) -> Option<DeepLinkAction> {
    if !config::e2e::enabled() {
        return None;
    }
    let query = url.strip_prefix(config::deep_link::SEND_SCHEME)?;
    let query = query.split('#').next().unwrap_or(query);
    let mut contact = None;
    let mut text = None;
    for part in query.split('&') {
        if let Some(value) = part.strip_prefix("to=") {
            contact = Some(value.trim());
        } else if let Some(value) = part.strip_prefix("text=") {
            text = Some(value.trim());
        }
    }
    let (contact, text) = (contact?, text?);
    (!contact.is_empty() && !text.is_empty()).then(|| DeepLinkAction::SendMessage {
        contact: Arc::from(contact),
        text: Arc::from(text),
    })
}

fn parse_action(url: &str) -> Option<DeepLinkAction> {
    if let Some(action) = parse_send_action(url) {
        return Some(action);
    }
    match share_urls::parse_share_url(url)? {
        ShareUrl::Contact { username, dht_key } => Some(DeepLinkAction::AddContact {
            username: Arc::<str>::from(username.as_ref()),
            dht_key: Arc::<str>::from(dht_key.as_ref()),
        }),
        ShareUrl::Pairing { payload } => Some(DeepLinkAction::Pair {
            payload: Arc::<str>::from(payload.as_ref()),
        }),
    }
}
