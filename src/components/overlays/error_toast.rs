//! Global error toast rendered by the root `ErrorBoundary`, plus the shared
//! [`ToastShell`] chrome used by every overlay toast.

use dioxus::prelude::*;

use crate::{
    components::shared::use_transient_message, config::ui::ERROR_TOAST_MS as AUTO_DISMISS_MS,
};

use super::TOP_STRIP_PADDING;

/// Visual style of a toast's alert box and default icon.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ToastKind {
    Error,
    Success,
    Info,
}

impl ToastKind {
    const fn alert_class(self) -> &'static str {
        match self {
            Self::Error => "alert alert-error alert-horizontal shadow-lg",
            Self::Success => "alert alert-success alert-horizontal shadow-lg",
            Self::Info => "alert alert-info alert-horizontal shadow-lg",
        }
    }

    const fn default_icon(self) -> &'static str {
        match self {
            Self::Error | Self::Info => "icon-[line-md--alert-circle-twotone-loop]",
            Self::Success => "icon-[line-md--confirm-circle-twotone]",
        }
    }

    /// Errors announce assertively; success/info notices are polite.
    const fn aria(self) -> (&'static str, &'static str) {
        match self {
            Self::Error => ("alert", "assertive"),
            Self::Success | Self::Info => ("status", "polite"),
        }
    }
}

/// Shared toast chrome: fixed top placement, `DaisyUI` alert box, and leading
/// icon. `icon_class` overrides the per-kind default icon.
#[component]
pub(super) fn ToastShell(
    kind: ToastKind,
    #[props(default)] icon_class: Option<&'static str>,
    children: Element,
) -> Element {
    let (role, live) = kind.aria();
    rsx! {
        // Top-center on mobile, top-right on sm+; slides down with a motion preset animation.
        // Starts below the activity bar's reserved strip, so a toast never covers the
        // theme and status controls it sits above in z-order.
        div {
            class: "{TOP_STRIP_PADDING} toast toast-top toast-center sm:toast-end z-1100 max-w-md p-4",
            role: role,
            "aria-live": live,
            div { class: kind.alert_class(),
                span { class: icon_class.unwrap_or_else(|| kind.default_icon()), "aria-hidden": "true" }
                {children}
            }
        }
    }
}

/// Displays a dismissible error alert that slides in from the top.
/// Non-actionable errors auto-dismiss after 8 seconds.
#[component]
pub fn ErrorToast(error: ErrorContext) -> Element {
    let error_for_text = error.clone();
    let error_text = use_memo(move || error_for_text.error().map(|err| err.to_string()));

    // Auto-dismiss only when the underlying error content changes. The
    // mirrored transient message re-arms on every new error; its stale-
    // generation guard ensures a timer can only expire the newest error.
    let (message, mut show_message) = use_transient_message(AUTO_DISMISS_MS);
    use_effect(move || {
        if let Some(err) = error_text() {
            show_message.set(err);
        }
    });
    // Once the transient expired (rather than being dismissed by hand), the
    // boundary must forget the error too — otherwise a later identical error
    // would not re-trigger the display effect above.
    use_effect({
        let error = error.clone();
        move || {
            if message().is_none() && error_text().is_some() {
                error.clear_errors();
            }
        }
    });

    rsx! {
        ToastShell { kind: ToastKind::Error,
            h3 { "Application Error" }
            div { class: "wrap-break-word",
                if let Some(err) = error_text() {
                    "{err}"
                }
            }
            button {
                class: "btn btn-sm btn-ghost btn-circle",
                "aria-label": "Dismiss error",
                onclick: move |_| error.clear_errors(),
                span {
                    class: "icon-[line-md--close-square-twotone]",
                    "aria-hidden": "true",
                }
            }
        }
    }
}
