//! Pure URL/deep-link builders, parsers, and shareable text formatters.
//!
//! No platform dependencies — only string manipulation.

use std::borrow::Cow;

use base64::engine::{Engine, general_purpose::URL_SAFE_NO_PAD};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

use crate::config;

/// Bytes left unescaped in a URL path segment: RFC 3986 unreserved
/// (`A-Z a-z 0-9 - . _ ~`). Everything else — including `/` and `?` — is
/// percent-encoded, matching the previous hand-rolled encoder byte-for-byte.
const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'=')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'|');

const CONTACT_SHARE_URL_PREFIX: &str = "https://po-app.pages.dev/c/";
const PAIRING_SHARE_URL_PREFIX: &str = "https://po-app.pages.dev/p/";
const CONTACT_SHARE_TOKEN_PREFIXES: &[&str] = &[
    config::deep_link::CHAT_SCHEME,
    CONTACT_SHARE_URL_PREFIX,
    "po:",
];
const PAIRING_SHARE_TOKEN_PREFIXES: &[&str] =
    &[config::deep_link::PAIR_SCHEME, PAIRING_SHARE_URL_PREFIX];

/// Parsed share URL/deep-link payload.
///
/// Simple path/query parses borrow from the source input; decode-heavy paths
/// (for example percent-decoded segments or contact deep links that unpack a
/// base64url payload) return owned strings instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareUrl<'a> {
    Contact {
        username: Cow<'a, str>,
        dht_key: Cow<'a, str>,
    },
    Pairing {
        payload: Cow<'a, str>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ShareBundle {
    pub title: Option<String>,
    pub username: Option<String>,
    pub deep_link: Option<String>,
    pub remaining_secs: Option<u64>,
}

impl ShareBundle {
    /// Builds a human-readable share message.
    ///
    /// When the provided link can be converted into a universal/app-link
    /// landing URL, the multiline bundle emits that path-based URL so it works
    /// in browsers and OS-level association files during rollout.
    #[must_use]
    pub fn to_multiline_text(&self) -> String {
        let mut parts = Vec::<String>::new();

        if let Some(title) = &self.title {
            parts.push(
                self.username
                    .as_ref()
                    .map_or_else(|| title.clone(), |name| format!("{title} — @{name}")),
            );
        }

        if let Some(link) = self
            .deep_link
            .as_deref()
            .and_then(canonical_share_url)
            .or_else(|| self.deep_link.clone())
        {
            parts.push(link);
        }

        // Expiry countdown only — PIN stays out-of-band.
        if let Some(remaining) = self.remaining_secs {
            parts.push(format!("Expires in {remaining}s"));
        }

        parts.join("\n\n")
    }
}

fn canonical_share_url(link: &str) -> Option<String> {
    match parse_share_url(link)? {
        ShareUrl::Contact { username, dht_key } => {
            Some(contact_share_url(username.as_ref(), dht_key.as_ref()))
        }
        ShareUrl::Pairing { payload } => Some(pairing_share_url(payload.as_ref())),
    }
}

fn trim_token(token: &str) -> &str {
    token
        .trim_start_matches(['(', '[', '{', '<', '"', '\''])
        .trim_end_matches([',', ';', '.', ')', ']', '}', '>', '"', '\''])
}

/// Iterates whitespace-separated words in pasted text that start with a known
/// share prefix, with common punctuation wrappers trimmed.
///
/// Matching is prefix-anchored (`starts_with`, not `contains`) so lookalike
/// words such as `Yahoo:` cannot shadow a real URL later in the text, and
/// every candidate is yielded so callers can skip ones that fail to parse.
fn share_token_candidates<'a>(
    text: &'a str,
    prefixes: &'static [&'static str],
) -> impl Iterator<Item = &'a str> {
    text.split_whitespace()
        .map(trim_token)
        .filter(move |word| prefixes.iter().any(|prefix| word.starts_with(prefix)))
}

/// Candidate contact share tokens (deep links, share URLs, `po:` compact
/// identities) found in a pasted text block.
pub fn contact_share_token_candidates(text: &str) -> impl Iterator<Item = &str> {
    share_token_candidates(text, CONTACT_SHARE_TOKEN_PREFIXES)
}

/// Candidate pairing share tokens (deep links, share URLs) found in a pasted
/// text block.
pub fn pairing_share_token_candidates(text: &str) -> impl Iterator<Item = &str> {
    share_token_candidates(text, PAIRING_SHARE_TOKEN_PREFIXES)
}

#[must_use]
fn percent_encode_path_segment(segment: &str) -> String {
    utf8_percent_encode(segment, PATH_SEGMENT_ENCODE_SET).to_string()
}

#[must_use]
fn percent_decode_path_segment(segment: &str) -> Option<String> {
    let decoded: Vec<u8> = percent_decode_str(segment).collect();
    String::from_utf8(decoded).ok()
}

fn maybe_percent_decode(segment: &str) -> Option<Cow<'_, str>> {
    if segment.contains('%') {
        percent_decode_path_segment(segment).map(Cow::Owned)
    } else {
        Some(Cow::Borrowed(segment))
    }
}

fn split_query_fragment(input: &str) -> &str {
    input
        .split('?')
        .next()
        .unwrap_or(input)
        .split('#')
        .next()
        .unwrap_or(input)
}

fn parse_contact_path(path: &str) -> Option<(Cow<'_, str>, Cow<'_, str>)> {
    let mut parts = split_query_fragment(path)
        .split('/')
        .filter(|segment| !segment.is_empty());
    let username = maybe_percent_decode(parts.next()?)?;
    let dht_key = maybe_percent_decode(parts.next()?)?;
    parts.next().is_none().then_some((username, dht_key))
}

fn parse_pairing_path(path: &str) -> Option<Cow<'_, str>> {
    let mut parts = split_query_fragment(path)
        .split('/')
        .filter(|segment| !segment.is_empty());
    let payload = maybe_percent_decode(parts.next()?)?;
    parts.next().is_none().then_some(payload)
}

fn parse_contact_deep_link(input: &str) -> Option<(String, String)> {
    let rest = input.strip_prefix(config::deep_link::CHAT_SCHEME)?;
    // Mail clients and trackers append `#fragment` — strip it before
    // splitting query parameters so `data=` stays clean.
    let query = rest.split('#').next().unwrap_or(rest);
    let encoded = query
        .split('&')
        .find_map(|part| part.strip_prefix("data="))?;
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let payload = str::from_utf8(&decoded).ok()?;
    let (username, dht_key) = payload.split_once('|')?;
    Some((username.trim().to_string(), dht_key.trim().to_string()))
}

fn parse_pairing_deep_link(input: &str) -> Option<Cow<'_, str>> {
    let rest = input.strip_prefix(config::deep_link::PAIR_SCHEME)?;
    let query = rest.split('#').next().unwrap_or(rest);
    query
        .split('&')
        .find_map(|part| part.strip_prefix("data="))
        .map(trim_token)
        .map(Cow::Borrowed)
}

/// Parses a contact or pairing share URL/deep link.
///
/// Accepts canonical universal/app-link paths and the existing `po://` scheme URLs.
#[must_use]
pub fn parse_share_url(input: &str) -> Option<ShareUrl<'_>> {
    let trimmed = trim_token(input.trim());

    if let Some((username, dht_key)) = parse_contact_deep_link(trimmed) {
        return Some(ShareUrl::Contact {
            username: Cow::Owned(username),
            dht_key: Cow::Owned(dht_key),
        });
    }
    if let Some(payload) = parse_pairing_deep_link(trimmed) {
        return Some(ShareUrl::Pairing { payload });
    }

    if let Some(path) = trimmed.strip_prefix(CONTACT_SHARE_URL_PREFIX)
        && let Some((username, dht_key)) = parse_contact_path(path)
    {
        return Some(ShareUrl::Contact { username, dht_key });
    }
    if let Some(path) = trimmed.strip_prefix(PAIRING_SHARE_URL_PREFIX)
        && let Some(payload) = parse_pairing_path(path)
    {
        return Some(ShareUrl::Pairing { payload });
    }

    None
}

/// Builds a contact share URL: `https://po-app.pages.dev/c/{username}/{dht_key}`
#[must_use]
pub fn contact_share_url(username: &str, dht_key: &str) -> String {
    format!(
        "{CONTACT_SHARE_URL_PREFIX}{}/{}",
        percent_encode_path_segment(username),
        percent_encode_path_segment(dht_key),
    )
}

/// Builds a pairing share URL: `https://po-app.pages.dev/p/{payload}`
#[must_use]
pub fn pairing_share_url(payload: &str) -> String {
    format!(
        "{PAIRING_SHARE_URL_PREFIX}{}",
        percent_encode_path_segment(payload),
    )
}

#[must_use]
pub fn pairing_deep_link(payload: &str) -> String {
    format!("{}data={payload}", config::deep_link::PAIR_SCHEME)
}

/// Builds a contact deep link: `po://chat?data=<base64url(username|dht_key)>`
#[must_use]
pub fn contact_deep_link(username: &str, dht_key: &str) -> String {
    let payload = format!("{username}|{dht_key}");
    format!(
        "{}data={}",
        config::deep_link::CHAT_SCHEME,
        URL_SAFE_NO_PAD.encode(payload)
    )
}

/// Builds the compact identity format: `po:username#dht_key`
#[must_use]
pub fn compact_identity(username: &str, dht_key: &str) -> String {
    format!("po:{username}#{dht_key}")
}
