//! User-domain validation helpers: credentials, onboarding, and device pairing.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Datelike, Local, NaiveDate};

use super::pairing_crypto::extract_pairing_payload;
use crate::config;

/// Returns `true` if `username` is valid.
///
/// Valid usernames are 3-256 characters and contain only valid ASCII.
/// Valid characters include alphanumerics, underscores, hyphens, and periods.
/// Rejects control characters, spaces, and empty-looking names.
#[must_use]
pub fn validate_username(username: &str) -> bool {
    (config::validation::USERNAME_MIN_LEN..=config::validation::USERNAME_MAX_LEN)
        .contains(&username.len())
        && username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Returns `true` if `password` is 8-256 chars and contains at least one
/// digit, lowercase, uppercase, and special character.
#[must_use]
pub fn validate_password(password: &str) -> bool {
    let mut flags = 0_u8;
    (config::validation::PASSWORD_MIN_LEN..=config::validation::PASSWORD_MAX_LEN)
        .contains(&password.chars().count())
        && !password.bytes().any(|b| b.is_ascii_control())
        && password.chars().any(|c| {
            flags |= match c {
                c if c.is_numeric() => 1,
                c if c.is_lowercase() => 2,
                c if c.is_uppercase() => 4,
                _ => 8,
            };
            flags == 0xF
        })
}

/// Returns `true` when `password` may be **created** as a new credential:
/// [`validate_password`] plus no invisible or non-retypeable characters.
///
/// Deliberately stricter than [`validate_password`], and deliberately split
/// from it rather than folded in: a credential already in use is *verified*,
/// never re-shaped. A legacy password that contains one of these characters
/// keeps working on every path that takes it as an existing credential
/// (backup restore, pairing join, remembered-session login), because those
/// call the structural check — or no shape check at all. Only paths that mint
/// a credential gate on this predicate: signup, password change, and any
/// pairing flow that sets one.
///
/// The asymmetry is intentional; do not "fix" it by widening
/// [`validate_password`].
#[must_use]
pub fn validate_new_password(password: &str) -> bool {
    validate_password(password) && !password.chars().any(is_credential_hazard)
}

/// Characters that must never appear in a **newly created** credential.
///
/// Three classes, all of them invisible or unretypeable: Unicode controls
/// (`Cc`, which covers the ASCII controls and `C1`), separators other than
/// the plain space the rule already accepts as a special character (`Zs`/`Zl`/
/// `Zp` — no-break spaces and the line/paragraph separators), and the `Cf`
/// format marks that real pastes carry (zero-width joiners, bidi marks and
/// isolates, word joiners, soft hyphen, BOM, and their rarer blocks).
///
/// The hazard is device-dependent input handling, not the character itself: a
/// credential containing one survives the paste on the device that created it
/// and is silently dropped or normalised away by another device's input path
/// or password manager, so the account has a password that works on exactly
/// one device. `Cf` is a closed standard class, so extending this list is a
/// data change on a Unicode version bump, not a new kind of hazard.
const fn is_credential_hazard(c: char) -> bool {
    c.is_control()
        || (c.is_whitespace() && c != ' ')
        || matches!(
            c,
            '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
}

/// Convert a YYYYMMDD integer to an HTML date input string (`YYYY-MM-DD`).
#[must_use]
pub fn yyyymmdd_to_input(value: i32) -> String {
    yyyymmdd_to_date(value).map_or_else(String::new, |date| date.format("%Y-%m-%d").to_string())
}

/// Parse an HTML date input string (`YYYY-MM-DD`) into a YYYYMMDD integer.
#[must_use]
pub fn input_to_yyyymmdd(value: &str) -> Option<i32> {
    if value.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(date_to_yyyymmdd)
}

/// Validate that a DOB is not in the future and represents an age between
/// 13 and 150 years. Returns a human-readable error if invalid, `None` if ok.
#[must_use]
pub fn validate_dob(yyyymmdd: i32) -> Option<&'static str> {
    let Some(dob) = yyyymmdd_to_date(yyyymmdd) else {
        return Some("Date of birth is required");
    };
    let today = Local::now().date_naive();
    if dob > today {
        return Some("Date of birth cannot be in the future");
    }
    let age = age_years(dob, today);
    if age < config::validation::MIN_AGE_YEARS {
        return Some("Must be at least 13 years old");
    }
    if age > config::validation::MAX_AGE_YEARS {
        return Some("Please enter a valid date of birth");
    }
    None
}

/// Validate that a PIN is 6 or more ASCII digits.
#[must_use]
pub fn validate_pin(pin: &str) -> bool {
    pin.len() >= config::validation::PIN_MIN_LEN && pin.chars().all(|c| c.is_ascii_digit())
}

/// Canonical password-rule hint shared by every password field.
#[must_use]
pub const fn password_rule_hint() -> &'static str {
    "8-256 chars, with digit, lower, upper & special"
}

/// Validates `password`, returning inline error text or `None` if valid.
#[must_use]
pub fn validate_password_msg(password: &str) -> Option<&'static str> {
    (!validate_password(password)).then_some(password_rule_hint())
}

/// Validates `password` as a **new** credential, returning inline error text or
/// `None` if valid.
///
/// Keeps [`password_rule_hint`] for an ordinary rule failure and gives the
/// invisible-character rejection its own wording: those characters cannot be
/// retyped by hand, so a generic hint would leave the user with a password they
/// cannot produce and no way to find what is wrong.
#[must_use]
pub fn validate_new_password_msg(password: &str) -> Option<&'static str> {
    if !validate_password(password) {
        return Some(password_rule_hint());
    }
    password
        .chars()
        .any(is_credential_hazard)
        .then_some("Password contains an invisible character — please retype it")
}

/// Validates `pin`, returning inline error text or `None` if valid.
#[must_use]
pub fn validate_pin_msg(pin: &str) -> Option<&'static str> {
    if validate_pin(pin) {
        None
    } else {
        Some("6 or more digits")
    }
}

/// Returns `true` if `payload` appears to be a valid pairing payload format.
#[must_use]
pub fn validate_pairing_payload(payload: &str) -> bool {
    let trimmed = payload.trim();
    if trimmed.len() > config::pairing::MAX_BASE64_PAYLOAD_LEN {
        return false;
    }
    let Some(payload_only) = extract_pairing_payload(trimmed) else {
        return false;
    };
    URL_SAFE_NO_PAD
        .decode(payload_only.as_ref())
        .is_ok_and(|decoded| decoded.len() >= config::pairing::MIN_PAYLOAD_LEN)
}

/// Returns `true` if `pin` is exactly the configured number of pairing digits.
#[must_use]
pub fn validate_pairing_pin(pin: &str) -> bool {
    pin.len() == config::validation::PAIRING_PIN_EXACT_LEN
        && pin.bytes().all(|b| b.is_ascii_digit())
}

/// Converts `YYYYMMDD` integer encoding to a date.
fn yyyymmdd_to_date(value: i32) -> Option<NaiveDate> {
    if value <= 0 {
        return None;
    }
    let year = value.div_euclid(10_000);
    let month = u32::try_from(value.div_euclid(100).rem_euclid(100)).ok()?;
    let day = u32::try_from(value.rem_euclid(100)).ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// Converts a date to `YYYYMMDD` integer encoding.
fn date_to_yyyymmdd(date: NaiveDate) -> Option<i32> {
    let month = i32::try_from(date.month()).ok()?;
    let day = i32::try_from(date.day()).ok()?;
    date.year()
        .checked_mul(10_000)?
        .checked_add(month.checked_mul(100)?)?
        .checked_add(day)
}

/// Exact full-year age between two calendar dates.
fn age_years(dob: NaiveDate, today: NaiveDate) -> i32 {
    let mut years = today.year() - dob.year();
    if (today.month(), today.day()) < (dob.month(), dob.day()) {
        years -= 1;
    }
    years
}
