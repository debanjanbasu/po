//! `LabeledField` — gpui port of `components/shared/form/*`.
//!
//! Preserves: label/placeholder layout, the inline validator-hint feedback
//! row (`FormFeedback` semantics via Theme), disabled state, and the exact
//! `ElementId`s the e2e harness drives (`login-username-input`,
//! `pin-gate-input`, …). The `use_form_field_manual` "Required" + validator
//! rule stays app-side in [`FormField`]; validation rules live in
//! `user::validation` unchanged.
//!
//! Value storage is the caller's keyed `Entity<EditableTextState>`. Password
//! and PIN fields use [`MaskedStorage`] (upstream has no masking — see the
//! fork-patch note) and hand the real value back through [`PasswordSink`].

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{
    App, ElementId, Entity, FontWeight, Hsla, IntoElement, ParentElement, RenderOnce, SharedString,
    Styled, WeakEntity, Window, px,
};
use gpui_ce_elements::editable_text::{
    EditableTextState, StringStorage, UnicodeTextStorage, text_input,
};

use crate::ui::theme::{Theme, shape};

/// Carried over from `form_feedback.rs`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    Success,
    Error,
}

/// The compact inline feedback row (old `FormFeedback { compact: true }`).
#[derive(Clone)]
pub struct Feedback {
    pub kind: FeedbackKind,
    pub message: SharedString,
}

impl Feedback {
    pub fn error(message: impl Into<SharedString>) -> Self {
        Self {
            kind: FeedbackKind::Error,
            message: message.into(),
        }
    }

    pub fn success(message: impl Into<SharedString>) -> Self {
        Self {
            kind: FeedbackKind::Success,
            message: message.into(),
        }
    }

    /// Theme-resolved color — never hardcoded (old `text-secondary` /
    /// `text-error`).
    pub const fn color(&self, theme: &Theme) -> Hsla {
        match self.kind {
            FeedbackKind::Success => theme.success_text(),
            FeedbackKind::Error => theme.error_text(),
        }
    }
}

/// App-side port of `use_form_field_manual`: value + dirty + error. Screens
/// own one per field and call [`Self::update`] from the state's change
/// subscription; error text is `&'static str` exactly like the old memo.
#[derive(Default)]
pub struct FormField {
    pub value: String,
    pub is_dirty: bool,
    /// `None` until dirty; `Some("Required")` when required and empty.
    pub error: Option<&'static str>,
}

impl FormField {
    pub fn update(
        &mut self,
        value: String,
        required: bool,
        validator: impl Fn(&str) -> Option<&'static str>,
    ) {
        self.value = value;
        self.is_dirty = true;
        self.error = if required && self.value.is_empty() {
            Some("Required")
        } else {
            validator(&self.value)
        };
    }
}

/// Hands the real (unmasked) value of a [`MaskedStorage`] field to its
/// screen. The element's `as_str()` sees bullets; only this sink sees the
/// secret.
#[derive(Clone, Default)]
pub struct PasswordSink(Rc<RefCell<String>>);

impl PasswordSink {
    pub fn value(&self) -> String {
        self.0.borrow().clone()
    }

    fn set(&self, value: String) {
        *self.0.borrow_mut() = value;
    }
}

/// Password/PIN storage: exposes a 1:1 bullet mask to the element while
/// mirroring every edit onto the real string.
///
/// Fork-patch note (2026-11): upstream `gpui_ce_elements` has no masking
/// (no `obscure`/`mask`/`secret` anywhere in `editable_text/`). This
/// wrapper relies on the mask being *character-for-character* (so UTF-8
/// byte offsets and UTF-16 positions in the masked view map 1:1 onto the
/// real string). True for credentials (ASCII-heavy); a combining-sequence
/// password would need a fork-patched masking mode in `EditableTextState`.
pub struct MaskedStorage {
    real: String,
    masked: String,
    /// Byte offset of each character start in `real`, plus the end offset.
    offsets: Vec<usize>,
    mirror: Rc<RefCell<String>>,
    version: u16,
}

impl MaskedStorage {
    const MASK: char = '•';

    pub fn with_sink(initial: &str, sink: PasswordSink) -> Self {
        let mut this = Self {
            real: initial.to_owned(),
            masked: String::new(),
            offsets: Vec::new(),
            mirror: sink.0,
            version: 0,
        };
        this.sync();
        this.mirror.borrow_mut().clone_from(&this.real);
        this
    }

    fn sync(&mut self) {
        self.masked = self.real.chars().map(|_| Self::MASK).collect();
        self.offsets = self
            .real
            .char_indices()
            .map(|(i, _)| i)
            .chain([self.real.len()])
            .collect();
    }
}

impl UnicodeTextStorage for MaskedStorage {
    fn version(&self) -> u16 {
        self.version
    }

    fn content_utf8(&self) -> &str {
        &self.masked
    }

    fn len_utf16(&self) -> usize {
        self.masked.chars().map(char::len_utf16).sum()
    }

    // The `str[..idx]` slices below are guarded by the `floor` helper above
    // them (indices are forced to char boundaries before every slice).
    #[allow(clippy::string_slice)]
    fn replace_range(&mut self, range: Range<usize>, text: &str) {
        // `range` addresses the masked view; map it onto the real string.
        // Offsets come from the element's own navigation over this storage,
        // so they land on char boundaries — but floor them defensively
        // rather than panic on a boundary split.
        let floor = |mut b: usize| {
            while b > 0 && !self.masked.is_char_boundary(b) {
                b -= 1;
            }
            b
        };
        let start_char = self.masked[..floor(range.start)].chars().count();
        let end_char = start_char
            + self.masked[floor(range.start)..floor(range.end)]
                .chars()
                .count();
        let real_start = self
            .offsets
            .get(start_char)
            .copied()
            .unwrap_or(self.real.len());
        let real_end = self
            .offsets
            .get(end_char)
            .copied()
            .unwrap_or(self.real.len());
        self.real.replace_range(real_start..real_end, text);
        self.version = self.version.wrapping_add(1);
        self.sync();
        self.mirror.borrow_mut().clone_from(&self.real);
    }
}

/// Character-capped storage (upstream has no `max_length` — see the fork
/// note). Caps in characters; the HTML `maxlength` on the old inputs was
/// code-unit based, equivalent for the ASCII-heavy credentials here.
pub struct MaxLengthStorage<S> {
    inner: S,
    max_chars: usize,
}

impl<S: UnicodeTextStorage> MaxLengthStorage<S> {
    pub const fn new(inner: S, max_chars: usize) -> Self {
        Self { inner, max_chars }
    }
}

impl<S: UnicodeTextStorage> UnicodeTextStorage for MaxLengthStorage<S> {
    fn version(&self) -> u16 {
        self.inner.version()
    }

    fn content_utf8(&self) -> &str {
        self.inner.content_utf8()
    }

    fn len_utf16(&self) -> usize {
        self.inner.len_utf16()
    }

    #[allow(clippy::string_slice)] // indices are floored to char boundaries
    fn replace_range(&mut self, range: Range<usize>, text: &str) {
        let content = self.inner.content_utf8();
        let floor = |mut b: usize| {
            while b > 0 && !content.is_char_boundary(b) {
                b -= 1;
            }
            b
        };
        let start = floor(range.start.min(content.len()));
        let end = floor(range.end.min(content.len())).max(start);
        let removed = content[start..end].chars().count();
        let room = self
            .max_chars
            .saturating_sub(content.chars().count() - removed);
        let text: String = text.chars().take(room).collect();
        self.inner.replace_range(start..end, &text);
    }
}

/// Which storage flavor a field uses — [`LabeledField::create_state`] seeds
/// the matching concrete `EditableTextState` (the generic `StorageType`
/// parameter prevents returning one boxed entity for all four).
#[derive(Clone, Copy)]
enum StorageKind {
    Plain,
    Capped(usize),
    Masked,
    MaskedCapped(usize),
}

/// One labeled input row: label, the editable element, optional feedback.
#[derive(IntoElement)]
pub struct LabeledField {
    id: ElementId,
    label: SharedString,
    placeholder: SharedString,
    state: WeakEntity<EditableTextState>,
    feedback: Option<Feedback>,
    disabled: bool,
}

impl LabeledField {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        state: &Entity<EditableTextState>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            placeholder: placeholder.into(),
            state: state.downgrade(),
            feedback: None,
            disabled: false,
        }
    }

    pub fn feedback(mut self, feedback: Option<Feedback>) -> Self {
        self.feedback = feedback;
        self
    }

    /// Disables editing (upstream `accepts_input(false)`) and dims the text;
    /// what the PIN gate uses while cooling down.
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Create the keyed state for this field id. Masked fields additionally
    /// return the [`PasswordSink`] the screen reads the real value from.
    /// Four concrete arms — see [`StorageKind`].
    pub fn create_state(
        id: &ElementId,
        masked: bool,
        max_chars: Option<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> (Entity<EditableTextState>, Option<PasswordSink>) {
        let sink = masked.then(PasswordSink::default);
        let kind = match (masked, max_chars) {
            (false, None) => StorageKind::Plain,
            (false, Some(cap)) => StorageKind::Capped(cap),
            (true, None) => StorageKind::Masked,
            (true, Some(cap)) => StorageKind::MaskedCapped(cap),
        };
        let state = match kind {
            StorageKind::Plain => {
                EditableTextState::use_keyed_init(id.clone(), window, cx, |_, _| {
                    StringStorage::default()
                })
            }
            StorageKind::Capped(cap) => {
                EditableTextState::use_keyed_init(id.clone(), window, cx, move |_, _| {
                    MaxLengthStorage::new(StringStorage::default(), cap)
                })
            }
            StorageKind::Masked => {
                // `masked == true` constructed the sink above; the unreachable
                // encodes that invariant instead of an `expect`.
                let Some(state_sink) = sink.clone() else {
                    unreachable!("masked fields always carry a sink")
                };
                EditableTextState::use_keyed_init(id.clone(), window, cx, move |_, _| {
                    MaskedStorage::with_sink("", state_sink)
                })
            }
            StorageKind::MaskedCapped(cap) => {
                // Same invariant; the sink is shared with the returned handle,
                // hence the clone.
                let Some(state_sink) = sink.clone() else {
                    unreachable!("masked fields always carry a sink")
                };
                EditableTextState::use_keyed_init(id.clone(), window, cx, move |_, _| {
                    MaxLengthStorage::new(MaskedStorage::with_sink("", state_sink), cap)
                })
            }
        };
        (state, sink)
    }

    /// The field's real value. Masked fields read the sink (element-side
    /// `as_str` returns bullets); plain fields read the state directly.
    pub fn value(
        state: &Entity<EditableTextState>,
        sink: Option<&PasswordSink>,
        cx: &App,
    ) -> String {
        sink.map_or_else(|| state.read(cx).as_str().to_owned(), PasswordSink::value)
    }
}

impl RenderOnce for LabeledField {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::for_appearance(cx);
        let is_error = self
            .feedback
            .as_ref()
            .is_some_and(|f| f.kind == FeedbackKind::Error);

        let border = if is_error {
            theme.error.fill
        } else {
            theme.input_border()
        };
        let text = if self.disabled {
            theme.text_dim()
        } else {
            theme.base_content
        };

        let mut input = text_input(self.id.clone())
            .placeholder(self.placeholder.clone())
            .state(self.state)
            .h(px(32.0)) // old `input-sm` ≈ 2rem
            .w_full()
            .px_2()
            .rounded(shape::RADIUS_FIELD)
            .border_1()
            .border_color(border)
            .bg(theme.input_bg())
            .text_color(text)
            .placeholder_color(theme.text_dim())
            .caret_color(theme.base_content)
            .selection_color(theme.primary.fill)
            .font_family(SharedString::from("IBM 3270"));
        if self.disabled {
            input = input.accepts_input(false);
        }

        gpui::div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .child(
                gpui::div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.base_content)
                    .child(self.label),
            )
            .child(input)
            .children(self.feedback.map(|f| {
                let color = f.color(theme);
                gpui::div().text_xs().text_color(color).child(f.message)
            }))
    }
}
