//! `FormFieldHandle` + `use_form_field_manual`: value/dirty/error/valid state for
//! manually validated form fields.

use std::rc::Rc;

use dioxus::prelude::*;

#[derive(Clone, Copy)]
pub struct FormFieldHandle {
    pub value: Signal<String>,
    pub error: Memo<Option<&'static str>>,
    pub is_valid: Memo<bool>,
    pub is_dirty: Signal<bool>,
}

pub fn use_form_field_manual(
    initial: &str,
    required: bool,
    validator: impl Fn(&str) -> Option<&'static str> + 'static,
) -> FormFieldHandle {
    let value = use_signal(|| initial.to_owned());
    let is_dirty = use_signal(|| false);
    let validator = Rc::new(validator);

    let error = use_memo(move || {
        let v = value.read();
        if !is_dirty() {
            return None;
        }
        if required && v.is_empty() {
            return Some("Required");
        }
        validator(&v)
    });

    let is_valid = use_memo(move || !value.read().is_empty() && is_dirty() && error().is_none());

    FormFieldHandle {
        value,
        error,
        is_valid,
        is_dirty,
    }
}
