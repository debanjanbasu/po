//! Shared widget builders (the port of `components/shared`).
//!
//! Every builder takes [`Theme`]-resolved colors as params (never hardcodes),
//! preserves the exact e2e seam `ElementId`s, and keeps the old
//! `FormFeedback`/`CenteredPage`/`ModalDialog`/toast semantics.

pub mod button;
pub mod field;
pub mod modal;
pub mod page;
pub mod toast;
