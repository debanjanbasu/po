//! Render helpers for validated password/PIN/date inputs: shared hint visibility
//! and oninput wiring over `FormFieldHandle`.

use crate::config::validation::PIN_MIN_LEN;

use dioxus::prelude::*;

use super::use_form_field::FormFieldHandle;

fn shared_oninput(
    value: Signal<String>,
    is_dirty: Signal<bool>,
) -> impl FnMut(FormEvent) + 'static {
    let mut value = value;
    let mut is_dirty = is_dirty;
    move |evt: FormEvent| {
        value.set(evt.value());
        is_dirty.set(true);
    }
}

fn render_hint(text: Option<&str>) -> Element {
    text.map_or_else(
        || rsx! {},
        |text| {
            rsx! {
                p { class: "validator-hint", "{text}" }
            }
        },
    )
}

pub fn render_validated_password(
    handle: &FormFieldHandle,
    label: &str,
    autocomplete: Option<&str>,
    id: Option<&str>,
) -> Element {
    let oninput = shared_oninput(handle.value, handle.is_dirty);
    let error = (handle.error)();

    rsx! {
        div { class: "w-full",
            input {
                id: id,
                class: "input input-sm validator w-full",
                class: if error.is_some() { "validator-error" },
                r#type: "password",
                value: "{handle.value}",
                required: true,
                oninput,
                "aria-label": label,
                placeholder: label,
                autocomplete: autocomplete.unwrap_or("off"),
            }
            {render_hint(error)}
        }
    }
}

pub fn render_validated_pin(handle: &FormFieldHandle, label: &str, id: Option<&str>) -> Element {
    let oninput = shared_oninput(handle.value, handle.is_dirty);
    let error = (handle.error)();
    rsx! {
        div { class: "w-full",
            input {
                id: id,
                class: "input input-sm validator w-full",
                class: if error.is_some() { "validator-error" },
                r#type: "password",
                inputmode: "numeric",
                value: "{handle.value}",
                pattern: "[0-9]*",
                minlength: PIN_MIN_LEN,
                oninput,
                "aria-label": label,
                placeholder: label,
            }
            {render_hint(error)}
        }
    }
}

pub fn render_validated_date(handle: &FormFieldHandle, label: &str, hint: Option<&str>) -> Element {
    let oninput = shared_oninput(handle.value, handle.is_dirty);
    let error = (handle.error)();

    rsx! {
        div { class: "w-full",
            label {
                class: "input input-sm validator w-full",
                class: if error.is_some() { "validator-error" },
                span { class: "label-text opacity-60", "{label}" }
                input {
                    r#type: "date",
                    value: "{handle.value}",
                    required: true,
                    oninput,
                }
            }
            {render_hint(error.or(hint))}
        }
    }
}
