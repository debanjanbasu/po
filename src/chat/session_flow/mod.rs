//! Contact add/undo and conversation bootstrap session flows.

use crate::{
    chat::contact::{ChatContact, ContactList},
    config,
    helpers::store_lock::lock_store,
    platform::share_urls::{self, ShareUrl},
    storage::{
        background_errors::report_nonfatal,
        store_trait::{self, CrdtModel},
    },
    user::{ctx::UserCtx, validation::validate_username},
    veilid::core::parse_record_key,
};
use anyhow::{Error as AnyhowError, Result as AnyhowResult, anyhow};
use dioxus::core::spawn_forever;
use thiserror::Error;

mod flow;

pub(in crate::chat) use flow::{activate_contact_for_conversation, upsert_conversation_entry};
pub use flow::{
    finalize_conversation_bootstrap, purge_canceled_contact_work_for_account, retry_pending_contact,
};

/// Errors from parsing contact input strings.
#[derive(Debug, Error)]
pub enum ChatInputError {
    #[error("Invalid format. expected: username|dht_key")]
    InvalidFormat,
    #[error("Input too long")]
    InputTooLong,
    #[error("Invalid username")]
    InvalidUsername,
    #[error("Not logged in")]
    NotLoggedIn,
    #[error("Contact already exists")]
    ContactExists,
    #[error("Invalid DHT key format")]
    InvalidDhtKey,
    #[error("Cannot add yourself as a contact")]
    SelfContact,
    #[error("Complete onboarding first — no DHT identity yet")]
    NoIdentity,
    #[error("Save failed: {0}")]
    SaveFailed(#[from] AnyhowError),
}

fn normalize_contact_parts<'a>(
    username: &'a str,
    dht_key: &'a str,
) -> Result<(&'a str, &'a str), ChatInputError> {
    let username = username.trim();
    let dht_key = dht_key.trim();
    if username.is_empty() || dht_key.is_empty() {
        return Err(ChatInputError::InvalidFormat);
    }
    if !validate_username(username) {
        return Err(ChatInputError::InvalidUsername);
    }
    Ok((username, dht_key))
}

pub fn parse_contact_input(input: &str) -> Result<(String, String), ChatInputError> {
    let trimmed = input.trim();

    if trimmed.len() > config::chat::MAX_CONTACT_INPUT_LEN {
        return Err(ChatInputError::InputTooLong);
    }

    // Pasted share messages wrap the real token in prose (title lines, expiry
    // notes, lookalike words). Try every recognized candidate — one that
    // fails to parse must not mask a valid one later in the text.
    for token in share_urls::contact_share_token_candidates(trimmed) {
        if let Ok(parsed) = parse_contact_token(token) {
            return Ok(parsed);
        }
    }

    parse_contact_token(trimmed)
}

/// Parses one candidate token: share URL/deep link, `po:user#key` compact
/// identity, or plain `username|dht_key`.
fn parse_contact_token(token: &str) -> Result<(String, String), ChatInputError> {
    if let Some(ShareUrl::Contact { username, dht_key }) = share_urls::parse_share_url(token) {
        let (username, dht_key) = normalize_contact_parts(username.as_ref(), dht_key.as_ref())?;
        return Ok((username.to_string(), dht_key.to_string()));
    }

    if let Some(rest) = token.strip_prefix("po:") {
        let Some((username, dht_key)) = rest.split_once('#') else {
            return Err(ChatInputError::InvalidFormat);
        };
        let (username, dht_key) = normalize_contact_parts(username, dht_key)?;
        return Ok((username.to_string(), dht_key.to_string()));
    }

    let mut parts = token.split('|');
    let Some(username) = parts.next() else {
        return Err(ChatInputError::InvalidFormat);
    };
    let Some(dht_key) = parts.next() else {
        return Err(ChatInputError::InvalidFormat);
    };

    if parts.next().is_some() {
        return Err(ChatInputError::InvalidFormat);
    }

    let (username, dht_key) = normalize_contact_parts(username, dht_key)?;
    Ok((username.to_string(), dht_key.to_string()))
}

/// Adds a contact from structured username + DHT key input.
/// Creates a conversation DHT record, adds the contact to the local list,
/// and enqueues sync operations.
/// Uses store locks to prevent concurrent mutation races.
pub async fn add_contact(username: &str, dht_key_str: &str) -> Result<(), ChatInputError> {
    let (username, dht_key_str) = normalize_contact_parts(username, dht_key_str)?;

    if parse_record_key(dht_key_str).is_err() {
        return Err(ChatInputError::InvalidDhtKey);
    }

    let ctx = UserCtx::try_current().ok_or(ChatInputError::NotLoggedIn)?;

    if username == ctx.username.as_ref() || dht_key_str == ctx.dht_key.as_ref() {
        return Err(ChatInputError::SelfContact);
    }

    if !ctx.has_identity() {
        return Err(ChatInputError::NoIdentity);
    }

    let _guard = lock_store(ctx.storage_key()).await;

    let mut contact_list = store_trait::load_local::<ContactList>(ctx.storage_key())
        .await
        .map_err(ChatInputError::SaveFailed)?;

    if contact_list
        .contacts
        .values()
        .any(|contact| &*contact.username == username)
        || contact_list.find_by_dht_key(dht_key_str).is_some()
    {
        return Err(ChatInputError::ContactExists);
    }

    let contact = ChatContact::new(username, dht_key_str, username);
    contact_list.add_contact(contact);
    <ContactList as CrdtModel>::persist_and_sync(&contact_list, &ctx)
        .await
        .map_err(ChatInputError::SaveFailed)?;
    flow::clear_contact_work_cancellation(&ctx, dht_key_str);

    if ctx.keypair.is_some() {
        let ctx_clone = ctx.clone();
        let username = username.to_string();
        let dht_key_str = dht_key_str.to_string();
        // Root-owned: the add-contact dialog closes the moment this returns Ok,
        // and a scope-owned task would be dropped with it — leaving the contact
        // stored but never bootstrapped.
        spawn_forever(async move {
            if let Err(e) =
                flow::setup_conversation_local(&ctx_clone, &username, &dht_key_str).await
            {
                report_nonfatal(
                    "chat.session_flow",
                    "Failed to set up local conversation",
                    e,
                );
            }
        });
    }

    Ok(())
}

/// Adds a contact from a text input string.
pub async fn add_contact_from_input(input: &str) -> Result<(), ChatInputError> {
    let (username, dht_key_str) = parse_contact_input(input)?;
    add_contact(&username, &dht_key_str).await
}

/// Undoes a recently added contact by removing it from the local contact list.
pub async fn undo_add_contact(input: &str) -> AnyhowResult<()> {
    let (_, dht_key_str) = parse_contact_input(input).map_err(|e| anyhow!("parse failed: {e}"))?;
    let ctx = UserCtx::try_current().ok_or_else(|| anyhow!("not logged in"))?;

    let manifest_key = {
        let _guard = lock_store(ctx.storage_key()).await;
        let mut contact_list = store_trait::load_local::<ContactList>(ctx.storage_key()).await?;
        let manifest_key = contact_list
            .find_by_dht_key(&dht_key_str)
            .and_then(|contact| {
                (!contact.conversation_record_key.is_empty())
                    .then(|| contact.conversation_record_key.clone())
            });
        if contact_list.remove_by_dht_key(&dht_key_str) {
            <ContactList as CrdtModel>::persist_and_sync(&contact_list, &ctx).await?;
        }
        flow::mark_contact_work_canceled(&ctx, &dht_key_str);
        manifest_key
    };
    flow::cancel_contact_queue_work(&ctx, &dht_key_str, manifest_key.as_deref()).await?;
    flow::cleanup_local_conversation_state(&ctx, &dht_key_str, manifest_key.as_deref()).await?;
    Ok(())
}
