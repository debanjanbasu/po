//! Deep link URL interception and dispatch.
//!
//! On iOS/macOS and Android, native FFI surfaces the URL that opened the app
//! (either `po://...` or `https://oo-app.pages.dev/...`). Startup captures at
//! most one pending launch URL, route-level consumers may peek at the pending
//! route target, and only the designated screen owner may consume the typed
//! action payload. Apple launch handlers are installed before the event
//! loop runs so cold URLs survive until the reactive app state is ready.

use std::sync::Arc;

use crate::config;
use crate::helpers::signals::GlobalSignal;
use crate::platform::share_urls::{self, ShareUrl};
use tracing::warn;

/// The typed deep-link actions the app understands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeepLinkAction {
    /// Add a contact by identity link payload.
    AddContact {
        /// Contact's display username.
        username: Arc<str>,
        /// Contact's DHT record key.
        dht_key: Arc<str>,
    },
    /// Pair a device with the given pairing payload.
    Pair {
        /// The pairing payload string (QR/deep-link encoded).
        payload: Arc<str>,
    },
    /// Harness-only send: `po://send?to=<username>&text=<text>`.
    SendMessage {
        /// Target username.
        to: Arc<str>,
        /// Message text.
        text: Arc<str>,
    },
}

impl DeepLinkAction {
    /// The screen owner that may consume this action (the old `owner()`).
    pub const fn owner(&self) -> DeepLinkOwner {
        match self {
            Self::AddContact { .. } | Self::SendMessage { .. } => DeepLinkOwner::NavLayout,
            Self::Pair { .. } => DeepLinkOwner::Pairing,
        }
    }
}

/// Which screen may consume the pending action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeepLinkOwner {
    /// The authenticated shell (chat list / contacts).
    NavLayout,
    /// The pairing screen.
    Pairing,
}

static PENDING_ACTION: GlobalSignal<Option<DeepLinkAction>> = GlobalSignal::new(None);

/// Reads `take_pending_url()` once, parses it into a [`DeepLinkAction`], and
/// stores the result. Call this during startup hydration before any component
/// can consume the URL.
///
/// Returns `true` when a new typed deep-link action was queued.
pub fn consume_pending_action() -> bool {
    let url = take_pending_url();
    if url.is_empty() {
        return false;
    }
    let action = parse_action(&url);
    // The only observable that a native launch URL actually reached the app.
    // Never log the URL: a pairing link embeds the PIN.
    config::e2e::event!("deep_link", accepted = action.is_some());
    let Some(action) = action else {
        warn!("Ignored unsupported deep link");
        return false;
    };
    *PENDING_ACTION.write() = Some(action);
    true
}

/// Ingests a URL delivered by the OS through the gpui runtime's
/// `on_open_urls` edge (desktop `register_url_scheme`, web `?url=` handoff,
/// mobile native hosts). Returns `true` when a new typed action was queued.
pub fn ingest_url(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    let action = parse_action(url);
    // Same marker rationale as `consume_pending_action`. Never log the URL.
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
    if has_pending_action() {
        return false;
    }
    consume_pending_action()
}

/// Whether a parsed action is queued.
pub fn has_pending_action() -> bool {
    PENDING_ACTION.peek().is_some()
}

/// Whether the queued action is a pairing link.
pub fn has_pending_pairing() -> bool {
    matches!(*PENDING_ACTION.peek(), Some(DeepLinkAction::Pair { .. }))
}

/// Takes the queued action if `owner` matches (the designated-screen-owner
/// consume contract).
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
/// (Frozen-tree era: the gpui screens call [`take_pending_action`]
/// directly on mount.)
#[cfg(not(any(
    feature = "gpui-desktop",
    feature = "gpui-web",
    feature = "gpui-mobile"
)))]
pub fn use_claimed_action(owner: DeepLinkOwner) -> GlobalSignal<Option<DeepLinkAction>> {
    let claimed_action = GlobalSignal::new(None::<DeepLinkAction>);
    if let Some(action) = take_pending_action(owner) {
        *claimed_action.write() = Some(action);
    }
    claimed_action
}

/// Installs the Apple URL handler before launch (cold/warm `po://` capture).
/// Native FFI only — compiled behind the mobile-backend pin.
#[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
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

/// Non-Apple stub so callers compile without cfg noise; the gpui entry's
/// `on_open_urls` edge owns URL intake there.
#[cfg(not(all(any(target_os = "ios", target_os = "macos"), feature = "mobile")))]
pub const fn bootstrap_apple_url_handler() {}

/// Reads the pending URL from the native bridge (Android: the launch intent).
#[cfg(target_os = "android")]
fn take_pending_url() -> String {
    super::native_bridge::take_pending_deep_link_url()
}

/// Reads the pending URL from the Apple plugin (post-launch pump).
#[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
fn take_pending_url() -> String {
    let plugin = match DeepLinkPlugin::new() {
        Ok(plugin) => plugin,
        Err(error) => {
            report_nonfatal(
                "platform.deep_link",
                "Deep-link plugin unavailable",
                &anyhow::anyhow!(error),
            );
            return String::new();
        }
    };
    pending_url(&plugin)
}

/// No native URL pump: the gpui `on_open_urls` edge feeds [`ingest_url`].
#[cfg(not(any(
    target_os = "android",
    all(any(target_os = "ios", target_os = "macos"), feature = "mobile")
)))]
const fn take_pending_url() -> String {
    String::new()
}

/// Parses a harness-only `po://send?to=<username>&text=<text>` URL. Parsed
/// only while the e2e seam is on, so a shipped build ignores the link.
fn parse_send_action(url: &str) -> Option<DeepLinkAction> {
    if !config::e2e::enabled() || !url.starts_with(config::deep_link::SEND_SCHEME) {
        return None;
    }
    let query = url.strip_prefix(config::deep_link::SEND_SCHEME)?;
    let mut to = None;
    let mut text = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "to" => to = Some(percent_decode(value)),
            "text" => text = Some(percent_decode(value)),
            _ => {}
        }
    }
    Some(DeepLinkAction::SendMessage {
        to: Arc::from(to?.as_ref()),
        text: Arc::from(text?.as_ref()),
    })
}

/// Parses any supported URL into a typed action (send → contact share →
/// pairing share).
fn parse_action(url: &str) -> Option<DeepLinkAction> {
    if let Some(action) = parse_send_action(url) {
        return Some(action);
    }
    match share_urls::parse_share_url(url)? {
        ShareUrl::Contact { username, dht_key } => Some(DeepLinkAction::AddContact {
            username: Arc::from(username.as_ref()),
            dht_key: Arc::from(dht_key.as_ref()),
        }),
        ShareUrl::Pairing { payload } => Some(DeepLinkAction::Pair {
            payload: Arc::from(payload.as_ref()),
        }),
    }
}

/// Minimal percent-decoding for query values (`%XX` + `+` as space).
fn percent_decode(value: &str) -> String {
    let with_spaces = value.replace('+', " ");
    let mut out = Vec::with_capacity(with_spaces.len());
    let bytes = with_spaces.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
