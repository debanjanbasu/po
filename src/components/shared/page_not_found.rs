//! 404 page shown for unrecognised routes.

use crate::components::shared::CenteredPage;
use dioxus::prelude::*;

const NOT_FOUND_IMAGE: Asset = asset!("/assets/orig-content/vector-404-not-found.svg");

/// Displays the attempted route path for an unrecognised route.
#[component]
pub fn PageNotFound(route: Vec<String>) -> Element {
    rsx! {
        CenteredPage { contained: true,
            div { class: "text-center w-full max-w-sm",
                h1 {
                    "Oops! 404"
                }
                p {
                    "Looks like you've wandered into the void! I couldn't find what you were looking for. Attempted to navigate to: "
                    strong {
                        class: "text-error",
                        {route.join("/")}
                    }
                }
                img {
                    class: "max-w-sm w-full rounded-sm mt-4 mx-auto",
                    src: NOT_FOUND_IMAGE,
                    alt: "404 Not Found",
                }
            }
        }
    }
}
