//! Reusable UI primitives shared across pages.

use dioxus::prelude::*;

/// Centered container for page content.
///
/// Full-screen mode (default) uses a fixed-height scroll container
/// (`h-dvh overflow-y-auto`) so content that exceeds the viewport (e.g.
/// expanded `ToS`) scrolls instead of being clipped. Safe-area insets keep
/// content clear of the Dynamic Island / notch.
///
/// Contained mode fills the available layout height so flex-based pages can
/// constrain their own scroll regions; overflowing content remains scrollable
/// by the ancestor.
#[component]
pub fn CenteredPage(
    children: Element,
    /// Inner content width class.
    #[props(default = "max-w-sm")]
    width: &'static str,
    /// Render inside an existing layout instead of a full-screen shell.
    #[props(default = false)]
    contained: bool,
) -> Element {
    let shell_class = if contained {
        "h-full min-h-0 grid place-items-center px-4 py-6"
    } else {
        // The safe-area padding lives in `overrides.css`
        // (`.centered-page-full`): it is `env()` arithmetic, and the CSS
        // file is where project deviations belong.
        "centered-page-full h-dvh overflow-y-auto overscroll-none grid place-items-center px-4"
    };
    rsx! {
        div { class: shell_class,
            div { class: "w-full {width}", class: if contained { "h-full min-h-0" }, {children} }
        }
    }
}

/// Sizes for [`Spinner`], mapping to daisyUI `loading-*` size classes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpinnerSize {
    Xs,
    Md,
    Lg,
}

/// App-standard loading spinner: one shape (daisyUI `loading-spinner`) that
/// inherits the surrounding text color — only the size varies by context.
/// Use this instead of raw `loading loading-*` markup so every loading state
/// looks the same. (The chat typing indicator is deliberately not a
/// `Spinner`: it's a typing animation, not a loading state.)
#[component]
pub fn Spinner(
    #[props(default = SpinnerSize::Md)] size: SpinnerSize,
    /// Accessible label. Omit for decorative spinners that sit next to
    /// visible text — they render `aria-hidden` instead.
    #[props(default)]
    label: Option<&'static str>,
    /// Extra utility classes (spacing, opacity), appended after the base.
    #[props(default = "")]
    class: &'static str,
) -> Element {
    let role = label.map(|_| "status");
    let size_class = match size {
        SpinnerSize::Xs => "loading-xs",
        SpinnerSize::Md => "loading-md",
        SpinnerSize::Lg => "loading-lg",
    };
    rsx! {
        span {
            class: "loading loading-spinner {size_class} {class}",
            role: role,
            "aria-label": label,
            "aria-hidden": if label.is_none() { "true" },
        }
    }
}

/// "OR" divider followed by a ghost link (e.g. "Pair from another device").
#[component]
pub fn OrDividerLink(
    href: &'static str,
    label: &'static str,
    #[props(default = "")] icon_class: &'static str,
) -> Element {
    rsx! {
        div { class: "divider opacity-60", "OR" }
        Link { class: "btn btn-ghost w-full btn-sm", to: href,
            if !icon_class.is_empty() {
                span { class: "{icon_class} size-4", "aria-hidden": "true" }
            }
            "{label}"
        }
    }
}
