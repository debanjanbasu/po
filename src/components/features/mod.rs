//! Feature pages: implemented Chat/TOTP screens and coming-soon placeholders.

use crate::components::shared::CenteredPage;
use dioxus::prelude::*;

pub(super) mod chat;
pub(super) mod totp;

#[component]
fn Placeholder(title: &'static str) -> Element {
    rsx! {
        CenteredPage { contained: true,
            div { class: "text-center w-full max-w-sm",
                h2 { "{title}" }
                p { "Coming soon..." }
            }
        }
    }
}

#[component]
pub fn Ai() -> Element {
    rsx! { Placeholder { title: "AI" } }
}

#[component]
pub fn Calendar() -> Element {
    rsx! { Placeholder { title: "Calendar" } }
}

#[component]
pub fn Calls() -> Element {
    rsx! { Placeholder { title: "Calls" } }
}
