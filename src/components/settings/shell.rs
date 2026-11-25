//! Shared shells for settings pages: page container, header card, card.

use dioxus::prelude::*;

/// Narrow page container used by all settings pages.
#[component]
pub fn SettingsPage(children: Element) -> Element {
    rsx! {
        div { class: "mx-auto max-w-sm p-4 pb-20 space-y-6", {children} }
    }
}

/// A settings list card (`ul.list`); children are `li` rows.
#[component]
pub fn SettingsCard(children: Element) -> Element {
    rsx! {
        ul { class: "list bg-base-200 rounded-box shadow-md text-[0.75rem]", {children} }
    }
}

/// Page header card: icon + title (+ optional subtitle).
#[component]
pub fn SettingsHeader(
    icon: &'static str,
    title: &'static str,
    #[props(default)] subtitle: Option<&'static str>,
) -> Element {
    rsx! {
        SettingsCard {
            li { class: "list-row",
                span { class: icon, "aria-hidden": "true" }
                div {
                    // A heading, not a `div`: this is the only page-level title
                    // these four settings routes have, and screen-reader heading
                    // navigation needs something to land on. Tailwind preflight
                    // resets headline size/weight, so the card looks identical.
                    h1 { {title} }
                    if let Some(subtitle) = subtitle {
                        div { {subtitle} }
                    }
                }
            }
        }
    }
}
