//! Shared form toolkit: validated field handles, validated-input render helpers,
//! and `FormFeedback` (re-exported here as the canonical import surface).

mod field_wrappers;
mod form_feedback;
mod use_form_field;
mod use_form_submission;
mod use_transient_message;

pub use field_wrappers::{render_validated_date, render_validated_password, render_validated_pin};
pub use form_feedback::{FeedbackKind, FormFeedback};
pub use use_form_field::{FormFieldHandle, use_form_field_manual};
pub use use_form_submission::{AutoSubmitGate, apply_form_input};
pub use use_transient_message::{TransientSetter, use_transient_message};
