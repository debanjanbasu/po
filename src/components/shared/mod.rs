//! Reusable UI primitives shared across pages.

mod form;
mod modal_dialog;
mod page_not_found;
mod qr_acquire_buttons;
mod qr_scanner_overlay;
mod qr_scanner_overlay_native;
mod qr_share_card;
mod shared_widgets;
mod size_format;
mod theme_dropdown;
mod time_format;
mod window_chrome;

pub use form::{
    AutoSubmitGate, FeedbackKind, FormFeedback, FormFieldHandle, TransientSetter, apply_form_input,
    render_validated_date, render_validated_password, render_validated_pin, use_form_field_manual,
    use_transient_message,
};
pub use modal_dialog::{ModalDialog, close_modal_dialog, use_modal_dialog_id};
pub use page_not_found::PageNotFound;
pub use qr_acquire_buttons::QrAcquireButtons;
pub use qr_scanner_overlay::QrScannerOverlay;
pub use qr_share_card::QrShareCard;
pub use shared_widgets::{CenteredPage, OrDividerLink, RetryButton, Spinner, SpinnerSize};
pub use size_format::{format_binary_bytes, format_compact_count};
pub use theme_dropdown::ThemeDropdown;
pub use time_format::{
    format_local_ymdhm_from_millis, format_relative_time, format_relative_time_at, format_uptime,
};
pub use window_chrome::WindowChrome;
