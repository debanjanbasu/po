//! Admission finalization (the port of `components/auth/mod.rs`'s
//! admission UI glue): `admit_user` + error reporting, shared by the login
//! and signup flows.

use crate::user::{self, User};

/// Outcome of the admission pipeline for a login/signup attempt.
pub enum AdmissionUi {
    /// User admitted; the session phase machine takes over from here.
    Admitted,
    /// Attempt superseded — staged effects cleared; caller should bail out.
    Superseded,
    /// Admission failed (already reported); caller shows its own error UI.
    Failed,
}

/// Admits `user` unless the attempt was superseded while it ran; reports
/// failures under `domain` and returns [`AdmissionUi::Failed`].
pub async fn finalize_admission_ui(
    user: User,
    is_current: bool,
    domain: &'static str,
    context: &'static str,
) -> AdmissionUi {
    if !is_current {
        user::clear_pending_admission_effects();
        return AdmissionUi::Superseded;
    }
    match user::admit_user(user).await {
        Ok(()) => AdmissionUi::Admitted,
        Err(error) => {
            crate::storage::background_errors::report_nonfatal(domain, context, &error);
            AdmissionUi::Failed
        }
    }
}
