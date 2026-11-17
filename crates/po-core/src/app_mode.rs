//! Application-level navigation state machine.
//!
//! Which screen po shows (login, signup, chat, settings) is itself an
//! explicit FSM, so every way of navigating — a button, a deep link, or a
//! slash command typed in the chat — dispatches the same events.

use serde::{Deserialize, Serialize};

/// Which screen the app is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppMode {
    /// Sign in with an existing account.
    Login,
    /// Create a new account.
    Signup,
    /// The main chat surface.
    Chat,
    /// Application settings.
    Settings,
}

/// Navigation events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum AppEvent {
    /// Navigate to a mode (e.g. from a menu or `/settings`).
    ToggleMode {
        /// The target mode.
        to: AppMode,
    },
    /// A login was attempted.
    LoginAttempt {
        /// Account name.
        username: String,
        /// Account password.
        password: String,
    },
    /// A signup was attempted.
    SignupAttempt {
        /// Account name.
        username: String,
        /// Account password.
        password: String,
    },
    /// The user signed out.
    Logout,
}

/// Navigation side effects the host performs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "kebab-case")]
pub enum AppEffect {
    /// Switch to the given screen.
    SetMode {
        /// The new screen.
        mode: AppMode,
    },
    /// Show a transient message (error, notice).
    ShowMessage {
        /// The message text.
        text: String,
    },
    /// Create an account.
    RegisterAccount {
        /// Account name.
        username: String,
        /// Account password.
        password: String,
    },
    /// Sign in.
    Authenticate {
        /// Account name.
        username: String,
        /// Account password.
        password: String,
    },
    /// The user signed out.
    LoggedOut,
}

/// Apply a navigation event.
///
/// The reducer is pure: authentication itself is an [`AppEffect`] the host
/// performs, so the transition table stays trivially testable.
#[must_use]
pub fn transition(mode: &AppMode, event: &AppEvent) -> (AppMode, Vec<AppEffect>) {
    use AppEvent::{LoginAttempt, Logout, SignupAttempt, ToggleMode};
    use AppMode::{Chat, Login, Settings, Signup};

    let mut effects = Vec::new();
    let next = match event {
        ToggleMode { to } => {
            effects.push(AppEffect::SetMode { mode: *to });
            *to
        }
        // Credentials only matter on the auth screens; elsewhere a stray
        // attempt is a no-op with a notice.
        LoginAttempt { username, password } => match mode {
            Login | Signup => {
                effects.push(AppEffect::Authenticate {
                    username: username.clone(),
                    password: password.clone(),
                });
                effects.push(AppEffect::SetMode { mode: Chat });
                Chat
            }
            Chat | Settings => {
                effects.push(AppEffect::ShowMessage {
                    text: "already signed in".into(),
                });
                *mode
            }
        },
        SignupAttempt { username, password } => match mode {
            Login | Signup => {
                effects.push(AppEffect::RegisterAccount {
                    username: username.clone(),
                    password: password.clone(),
                });
                effects.push(AppEffect::SetMode { mode: Chat });
                Chat
            }
            Chat | Settings => {
                effects.push(AppEffect::ShowMessage {
                    text: "already signed in".into(),
                });
                *mode
            }
        },
        Logout => {
            effects.push(AppEffect::LoggedOut);
            effects.push(AppEffect::SetMode { mode: Login });
            Login
        }
    };
    (next, effects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigate_to_chat_from_login() {
        let (mode, effects) =
            transition(&AppMode::Login, &AppEvent::ToggleMode { to: AppMode::Chat });
        assert_eq!(mode, AppMode::Chat);
        assert_eq!(effects, vec![AppEffect::SetMode { mode: AppMode::Chat }]);
    }

    #[test]
    fn logout_returns_to_login() {
        let (mode, effects) = transition(&AppMode::Settings, &AppEvent::Logout);
        assert_eq!(mode, AppMode::Login);
        assert_eq!(
            effects,
            vec![AppEffect::LoggedOut, AppEffect::SetMode { mode: AppMode::Login }]
        );
    }

    #[test]
    fn signup_attempt_moves_to_chat_and_requests_registration() {
        let (mode, effects) = transition(
            &AppMode::Signup,
            &AppEvent::SignupAttempt {
                username: "ada".into(),
                password: "secret".into(),
            },
        );
        assert_eq!(mode, AppMode::Chat);
        assert!(effects
            .iter()
            .any(|e| matches!(e, AppEffect::RegisterAccount { username, .. } if username == "ada")));
    }

    #[test]
    fn login_attempt_requests_authentication() {
        let (mode, effects) = transition(
            &AppMode::Login,
            &AppEvent::LoginAttempt {
                username: "ada".into(),
                password: "secret".into(),
            },
        );
        assert_eq!(mode, AppMode::Chat);
        assert!(effects
            .iter()
            .any(|e| matches!(e, AppEffect::Authenticate { username, .. } if username == "ada")));
    }

    #[test]
    fn auth_attempts_while_signed_in_are_noops_with_a_notice() {
        let (mode, effects) = transition(
            &AppMode::Chat,
            &AppEvent::LoginAttempt {
                username: "ada".into(),
                password: "secret".into(),
            },
        );
        assert_eq!(mode, AppMode::Chat);
        assert!(matches!(&effects[..], [AppEffect::ShowMessage { .. }]));
    }
}
