//! Shared modal dialog shell with native `<dialog>` semantics.

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_arch = "wasm32")]
use std::{cell::RefCell, collections::HashMap};

use dioxus::prelude::*;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;
#[cfg(target_arch = "wasm32")]
use web_sys::{HtmlDialogElement, HtmlElement, window};

use crate::helpers::spawn;

static NEXT_MODAL_DIALOG_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(target_arch = "wasm32")]
thread_local! {
    static RETURN_FOCUS_STORE: RefCell<HashMap<String, Option<HtmlElement>>> =
        RefCell::new(HashMap::new());
}

#[must_use]
pub fn use_modal_dialog_id(prefix: &'static str) -> String {
    use_hook(move || {
        format!(
            "{prefix}-{}",
            NEXT_MODAL_DIALOG_ID.fetch_add(1, Ordering::Relaxed)
        )
    })
}

#[cfg(target_arch = "wasm32")]
fn open_modal_dialog(dialog_id: &str) {
    let Some(doc) = window().and_then(|w| w.document()) else {
        return;
    };
    let Some(dialog) = doc
        .get_element_by_id(dialog_id)
        .and_then(|el| el.dyn_into::<HtmlDialogElement>().ok())
    else {
        return;
    };
    RETURN_FOCUS_STORE.with(|store| {
        let mut map = store.borrow_mut();
        if !map.contains_key(dialog_id) {
            let active = doc
                .active_element()
                .and_then(|el| el.dyn_into::<HtmlElement>().ok());
            map.insert(dialog_id.to_string(), active);
        }
    });
    if dialog.open() {
        dialog.close();
    }
    if dialog.show_modal().is_err() {
        let _ = dialog.set_attribute("open", "");
    }
}

#[cfg(target_arch = "wasm32")]
pub fn close_modal_dialog(dialog_id: &str) {
    let previous = RETURN_FOCUS_STORE.with(|store| store.borrow_mut().remove(dialog_id).flatten());
    if let Some(doc) = window().and_then(|w| w.document())
        && let Some(dialog) = doc
            .get_element_by_id(dialog_id)
            .and_then(|el| el.dyn_into::<HtmlDialogElement>().ok())
    {
        if dialog.open() {
            dialog.close();
        }
        let _ = dialog.remove_attribute("open");
    }
    if let Some(el) = previous {
        let _ = el.focus();
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub const fn close_modal_dialog(_dialog_id: &str) {}

#[component]
pub fn ModalDialog(
    dialog_id: String,
    title: &'static str,
    on_close: EventHandler<()>,
    #[props(default = "")] panel_class: &'static str,
    #[props(default = "")] title_class: &'static str,
    #[props(default = false)] show_close_button: bool,
    #[props(default = false)] close_button_autofocus: bool,
    #[props(default = "Close dialog")] close_button_label: &'static str,
    #[props(default = false)] close_disabled: bool,
    children: Element,
) -> Element {
    let title_id = format!("{dialog_id}-title");
    let dialog_attr_id = dialog_id.clone();
    // If the parent unmounts while the dialog is open (e.g. navigation), close it
    // and drop the stored return-focus target so focus isn't stranded.
    let dialog_id_for_drop = dialog_id.clone();
    use_drop(move || {
        close_modal_dialog(&dialog_id_for_drop);
    });
    #[cfg(target_arch = "wasm32")]
    let dialog_id_for_mount = dialog_id.clone();
    let close = Callback::new(move |()| {
        if !close_disabled {
            close_modal_dialog(&dialog_id);
            on_close.call(());
        }
    });
    rsx! {
        dialog {
            id: "{dialog_attr_id}",
            // `rail-inset` mirrors the nav layout's rail inset: the dock rail is
            // permanent chrome and page content is centered in the region beside it,
            // so the panel centers on that same axis instead of the raw viewport.
            // The rule guards on the rail actually being present, because auth
            // routes render outside `NavLayout` (see `overrides.css`).
            class: "modal rail-inset",
            open: !cfg!(target_arch = "wasm32"),
            role: "dialog",
            "aria-modal": "true",
            "aria-labelledby": "{title_id}",
            tabindex: "-1",
            onmounted: move |ev| {
                // Open from the dialog's own mount event: running this from the
                // component body races the DOM insertion, and a missed lookup
                // silently leaves the dialog shut with no way to re-arm it.
                #[cfg(target_arch = "wasm32")]
                open_modal_dialog(&dialog_id_for_mount);
                if show_close_button && !close_button_autofocus {
                    let data = ev.data();
                    spawn(async move {
                        let _ = data.set_focus(true).await;
                    });
                }
            },
            onkeydown: move |event: KeyboardEvent| {
                if event.key() == Key::Named(NamedKey::Escape) {
                    event.prevent_default();
                    close.call(());
                }
            },
            oncancel: move |event| {
                event.prevent_default();
                close.call(());
            },
            onclick: move |_| close.call(()),

            div {
                class: "modal-box {panel_class}",
                onclick: move |event| event.stop_propagation(),

                div { class: "mb-4 flex items-start justify-between gap-3",
                    h2 {
                        id: "{title_id}",
                        class: "flex-1 {title_class}",
                        "{title}"
                    }
                    if show_close_button {
                        button {
                            r#type: "button",
                            class: "btn btn-sm btn-circle btn-ghost",
                            disabled: close_disabled,
                            autofocus: close_button_autofocus,
                            onmounted: move |ev| {
                                if close_button_autofocus {
                                    let data = ev.data();
                                    spawn(async move {
                                        let _ = data.set_focus(true).await;
                                    });
                                }
                            },
                            "aria-label": close_button_label,
                            onclick: move |_| close.call(()),
                            "\u{2715}"
                        }
                    }
                }

                {children}
            }
        }
    }
}
