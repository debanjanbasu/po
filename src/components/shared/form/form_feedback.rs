//! `FormFeedback` component: form-level success/error feedback as a `DaisyUI` alert
//! or a compact inline text row.

use dioxus::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    Success,
    Error,
}

#[derive(Clone, PartialEq, Eq, Props)]
pub struct FormFeedbackProps {
    pub kind: FeedbackKind,
    pub message: String,
    #[props(default = false)]
    pub compact: bool,
    /// Extra classes for the alert container (spacing, text sizing).
    #[props(default = "")]
    pub class: &'static str,
    /// Icon-font class overriding the built-in check/cross SVG icon.
    #[props(default)]
    pub icon_class: Option<&'static str>,
}

const fn feedback_class(kind: FeedbackKind, compact: bool) -> &'static str {
    if compact {
        match kind {
            FeedbackKind::Success => "text-[0.7rem] leading-4 font-medium text-secondary",
            FeedbackKind::Error => "text-[0.7rem] leading-4 text-error",
        }
    } else {
        match kind {
            FeedbackKind::Success => "alert alert-success",
            FeedbackKind::Error => "alert alert-error",
        }
    }
}

const fn feedback_icon_path(kind: FeedbackKind, compact: bool) -> Option<&'static str> {
    if compact {
        None
    } else {
        Some(match kind {
            FeedbackKind::Success => "M9 12l2 2 4-4m6 2a9 9 0 11-18 0 9 9 0 0118 0z",
            FeedbackKind::Error => {
                "M10 14l2-2m0 0l2-2m-2 2l-2-2m2 2l2 2m7-2a9 9 0 11-18 0 9 9 0 0118 0z"
            }
        })
    }
}

#[component]
pub fn FormFeedback(props: FormFeedbackProps) -> Element {
    let class = feedback_class(props.kind, props.compact);
    let role = match props.kind {
        FeedbackKind::Success => "status",
        FeedbackKind::Error => "alert",
    };
    let icon_path = feedback_icon_path(props.kind, props.compact);

    rsx! {
        div {
            class: "{class} {props.class}",
            role: role,
            if let Some(icon_class) = props.icon_class {
                span { class: "{icon_class} shrink-0", "aria-hidden": "true" }
            } else if let Some(icon_path) = icon_path {
                svg {
                    "xmlns": "http://www.w3.org/2000/svg",
                    class: "h-5 w-5 shrink-0 stroke-current",
                    fill: "none",
                    view_box: "0 0 24 24",
                    path {
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        stroke_width: "2",
                        d: "{icon_path}",
                    }
                }
            }
            span { "{props.message}" }
        }
    }
}
