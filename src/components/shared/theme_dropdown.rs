//! Reusable theme selector: 1-click cycle button (compact) or segmented join (settings).

use dioxus::prelude::*;

use crate::global_state::theme;

fn apply_theme(value: &str) {
    value.clone_into(&mut theme().write());
}

/// Theme selector with three options: System, Light, Dark.
///
/// When `compact` is true, renders a single 1-click button that cycles
/// `system -> light -> dark -> system` (for tight spaces like the activity bar).
/// When false, renders an inline 3-item segmented join (for settings pages).
#[component]
pub fn ThemeDropdown(#[props(default = false)] compact: bool) -> Element {
    let active = match theme().read().as_str() {
        "light" => "light",
        "dark" => "dark",
        _ => "system",
    };
    if compact {
        let (theme_icon, theme_label, next_theme) = match active {
            "light" => (
                "icon-[line-md--sunny-twotone] text-warning",
                "Theme: Light (click for Dark)",
                "dark",
            ),
            "dark" => (
                "icon-[line-md--moon-twotone] text-secondary",
                "Theme: Dark (click for System)",
                "system",
            ),
            _ => (
                "icon-[line-md--laptop-twotone] text-base-content/60",
                "Theme: System (click for Light)",
                "light",
            ),
        };
        return rsx! {
            button {
                r#type: "button",
                class: "btn btn-ghost btn-xs btn-circle",
                title: "{theme_label}",
                "aria-label": "{theme_label}",
                onclick: move |_| apply_theme(next_theme),
                span { class: "{theme_icon}", "aria-hidden": "true" }
            }
        };
    }

    rsx! {
        div { class: "join", role: "group", "aria-label": "Theme",
            for (value, icon, label) in [
                ("system", "icon-[line-md--laptop-twotone]", "System"),
                ("light", "icon-[line-md--sunny-twotone]", "Light"),
                ("dark", "icon-[line-md--moon-twotone]", "Dark"),
            ] {
                button {
                    id: "theme-option-{value}",
                    r#type: "button",
                    class: "btn btn-xs join-item gap-1",
                    class: if active == value { "btn-active" } else { "btn-ghost" },
                    "aria-pressed": if active == value { "true" } else { "false" },
                    onclick: move |_| apply_theme(value),
                    span { class: "{icon}", "aria-hidden": "true" }
                    "{label}"
                }
            }
        }
    }
}
