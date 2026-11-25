//! Pairing flow logic: state resets, share generation, and device login.

use std::borrow::Cow;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use dioxus::prelude::*;
use thiserror::Error;
use veilid_core::RecordKey;

use crate::{
    components::auth::LatestAttemptController,
    config::e2e,
    helpers::async_utils::now_epoch_secs,
    models::{app_state::AppState, pending_operation::OpType},
    platform::{
        qr::{self, generate_qr_webp},
        share_urls::{pairing_deep_link, pairing_share_url},
    },
    storage::{background_errors::report_nonfatal, pairing_sync_status},
    user::{
        self, NewDeviceLoginError, PairingError, PairingRun, clear_pending_admission_effects,
        ctx::UserCtx, decrypt_pairing_payload, extract_pairing_payload, generate_pairing_payload,
        pairing_beacon,
    },
    veilid::{core::parse_record_key, store::report_nonfatal_unless_network},
};

/// Generated pairing payload ready to share with the other device.
#[derive(Clone)]
pub(super) struct PairingShare {
    pub(super) qr_webp: Bytes,
    pub(super) pin: String,
    pub(super) deep_link: String,
    /// Correlates beacon milestones with this exact QR generation.
    pub(super) pairing_id: pairing_beacon::PairingId,
    /// Epoch seconds when this share was generated (stale-beacon filter).
    pub(super) generated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PairAttempt {
    pub(super) payload: String,
    pub(super) pin: String,
    pub(super) password: String,
    pub(super) remember_me: bool,
}

pub(super) struct PreparedPairing {
    pub(super) user: user::User,
    pub(super) dht_key: RecordKey,
    pub(super) app_state: AppState,
    pub(super) identity_docs: user::IdentitySeedDocs,
    pub(super) run: PairingRun,
}

pub(super) enum ShareGenerationResult {
    WrongPassword,
    NotOnboarded,
    Ready(PairingShare),
}

#[derive(Debug, Error)]
pub(super) enum PairDeviceError {
    #[error(transparent)]
    Pairing(#[from] PairingError),
    #[error(transparent)]
    DeviceLogin(#[from] NewDeviceLoginError),
}

impl PairDeviceError {
    pub(super) const fn cooldown_remaining_secs(&self) -> Option<u64> {
        match self {
            Self::Pairing(PairingError::Cooldown { remaining_secs, .. }) => Some(*remaining_secs),
            _ => None,
        }
    }

    /// Harness marker kind for a failed pairing attempt. `None` for causes the
    /// catalog does not name (malformed payload, network, internal) — those are
    /// not rejections of the credentials themselves.
    const fn rejection_kind(&self) -> Option<&'static str> {
        match self {
            Self::Pairing(PairingError::Expired) => Some("expired"),
            Self::Pairing(PairingError::InvalidCredentials) => Some("invalid_credentials"),
            Self::Pairing(PairingError::Cooldown { .. }) => Some("cooldown"),
            _ => None,
        }
    }
}
pub(super) fn reset_pair_state(
    mut scan_error: Signal<Option<Cow<'static, str>>>,
    mut pair_error: Signal<Option<Cow<'static, str>>>,
    pair_attempt: LatestAttemptController,
) {
    if scan_error.peek().is_some() {
        scan_error.set(None);
    }
    if pair_error.peek().is_some() {
        pair_error.set(None);
    }
    if pair_attempt.is_busy() {
        // The in-flight attempt is dropped because its inputs changed or a
        // newer attempt replaced it: rejected as superseded.
        e2e::event!("pairing_rejected", reason = "superseded");
        pair_attempt.reset(clear_pending_admission_effects);
        pairing_sync_status::clear_active();
    }
}

pub(super) fn normalize_pairing_input(input: &str) -> String {
    extract_pairing_payload(input)
        .unwrap_or(Cow::Borrowed(input))
        .into_owned()
}

pub(super) async fn generate_share(password: &str) -> Result<ShareGenerationResult> {
    let (dht_key, keypair, username, account_id) = {
        let ctx = UserCtx::try_current().context("Not logged in")?;
        if !ctx.has_identity() {
            return Ok(ShareGenerationResult::NotOnboarded);
        }
        let dht_key = parse_record_key(&ctx.dht_key).map_err(|_| anyhow!("Invalid DHT key"))?;
        let keypair = ctx.keypair.context("Keypair not decrypted")?;
        (
            dht_key,
            keypair,
            ctx.username.to_string(),
            ctx.account_id.to_string(),
        )
    };

    // Verify once; the payload below is keyed from the password itself, and the
    // salt form it reports tells the joining device how to derive the DHT secret.
    let Some(form) = user::verify_password(&username, password).await? else {
        return Ok(ShareGenerationResult::WrongPassword);
    };

    // The joining device pulls everything from the DHT by keys derived from
    // the shared keypair, so the only hard requirement at QR time is that the
    // identity record itself is readable (verified below). Child surfaces
    // (TOTP, contacts, conversation index) can land later: the joining device
    // re-pulls them, and this device keeps syncing while both stay online.
    //
    // Still refresh every app-owned surface — a sync op that dead-lettered
    // during an offline window would otherwise leave the network copy stale
    // forever — but through the sync queue, not a direct push: the queue
    // squashes these with any pending duplicates and serializes them with
    // steady-state sync, so pairing never races the watcher on the same
    // subkeys (concurrent writers force reconcile conflict-retry rounds that
    // can blow the record-scope timeout). Each op pushes current local truth,
    // so a fresh enqueue supersedes whatever a dead-lettered twin missed.
    if let Some(ctx) = UserCtx::try_current() {
        for op_type in [
            OpType::ProfileAndSecurity,
            OpType::AppState,
            OpType::Totp,
            OpType::ChatContacts,
            OpType::Chat,
        ] {
            if let Err(e) = ctx.enqueue_touch(op_type).await {
                report_nonfatal("auth.pairing", "Failed to enqueue pre-pairing sync op", &e);
            }
        }
    }

    // Deliberately no network verification here: the payload below is pure
    // local crypto, so the QR renders in the time one KDF takes. Publish
    // verification/repair runs in the background while the QR is on screen
    // (see [`verify_publish_warning`]) — the human scanning + typing the PIN
    // gives it a 10-20s head start before the other device pulls anything.
    let (payload, pin) =
        generate_pairing_payload(&dht_key, &keypair, &account_id, password, form).await?;
    let pairing_id = pairing_beacon::pairing_id_for_payload(&payload)?;
    let deep_link = pairing_deep_link(&payload);
    // QR encodes the https share URL so external camera apps open the landing
    // page (which hands off to the app); in-app scanners parse the same URL.
    let qr_link = pairing_share_url(&payload);
    let qr_webp = generate_qr_webp(qr_link.clone()).await?;

    // Harness seam: the share screen is the only place this PIN exists, so a
    // tandem run reads it from stdout instead of scraping the UI. Emitted only
    // with PO_E2E=1 in a debug build (the accessor is `false` in release), and
    // the account password never appears here. See docs/device-testing.md.
    e2e::event!("share", pin = %pin, deep_link = %deep_link);
    // Prove the rendered QR actually encodes the share URL (see
    // `report_qr_round_trip`).
    if e2e::enabled() {
        qr::report_qr_round_trip(&qr_webp, &qr_link).await;
    }

    Ok(ShareGenerationResult::Ready(PairingShare {
        qr_webp,
        pin,
        deep_link,
        pairing_id,
        generated_at: now_epoch_secs(),
    }))
}

/// Background companion to [`generate_share`]: verifies the identity record
/// the QR points at is readable on the network and repairs under-replicated
/// subkeys. The QR is already on screen when this runs, so problems surface
/// as a warning under it instead of blocking generation — the joining device
/// retries its pulls anyway, so a slow publish usually self-heals.
pub(super) async fn verify_publish_warning() -> Option<String> {
    let (dht_key, keypair) = {
        let ctx = UserCtx::try_current()?;
        let dht_key = parse_record_key(&ctx.dht_key).ok()?;
        let keypair = ctx.keypair?;
        (dht_key, keypair)
    };
    match user::record::ensure_identity_published(&dht_key, &keypair).await {
        Ok(missing) if missing.is_empty() => None,
        Ok(missing) => Some(format!(
            "Your account isn't fully readable on the network yet (waiting on: {}). Keep this device online — pairing may stall until this clears.",
            missing.join(", "),
        )),
        Err(e) => {
            // Transient network trouble (offline window, flapping) is a
            // warning, not an error: the retries inside the publish check
            // already waited it out once, and the joining device re-pulls
            // on its own anyway.
            let surfaced = report_nonfatal_unless_network(
                "auth.pairing",
                "Identity publish verification failed",
                &e,
            );
            Some(if surfaced {
                "Couldn't verify your account on the network. Keep this device online while pairing."
                    .to_string()
            } else {
                "Your connection is unstable, so your account couldn't be verified on the network. Keep this device online while pairing."
                    .to_string()
            })
        }
    }
}

pub(super) fn pair_error_message(error: &PairDeviceError) -> Cow<'static, str> {
    match error {
        PairDeviceError::Pairing(PairingError::Expired) => {
            "This pairing code has expired. Generate a new one and try again.".into()
        }
        PairDeviceError::Pairing(PairingError::InvalidCredentials) => {
            "Incorrect pairing PIN or account password.".into()
        }
        PairDeviceError::Pairing(PairingError::Cooldown {
            attempts,
            remaining_secs,
        }) => format!(
            "Too many failed pairing attempts ({attempts}). Wait {remaining_secs}s before trying again."
        )
        .into(),
        PairDeviceError::Pairing(
            PairingError::InvalidBase64Payload(_)
            | PairingError::PayloadTooShort
            | PairingError::EmptyPayload
            | PairingError::UnsupportedVersion(_)
            | PairingError::MalformedPayload(_)
            | PairingError::InvalidPayloadBytes
            | PairingError::PayloadTooLarge,
        ) => "Invalid pairing data. Re-scan or paste a fresh pairing code.".into(),
        PairDeviceError::Pairing(PairingError::InvalidPin) => {
            "PIN must be exactly 12 digits.".into()
        }
        PairDeviceError::DeviceLogin(NewDeviceLoginError::RecordUnreachable(_)) => {
            "Couldn't reach this account's sync record on the network. Check this device's connection, keep the account's other device online, and try again."
                .into()
        }
        PairDeviceError::DeviceLogin(NewDeviceLoginError::NetworkNotReady(_)) => {
            "This device couldn't connect to the sync network in time. Check its internet connection, wait a few seconds, and try again."
                .into()
        }
        PairDeviceError::DeviceLogin(NewDeviceLoginError::SourceStale(_)) => {
            "Your other device hasn't finished publishing this account's data yet. Keep that device open and online, then try again."
                .into()
        }
        PairDeviceError::Pairing(PairingError::Internal(_))
        | PairDeviceError::DeviceLogin(NewDeviceLoginError::Internal(_)) => {
            "Unable to pair with that data right now. Please verify the details and try again."
                .into()
        }
        PairDeviceError::DeviceLogin(NewDeviceLoginError::ConflictingLocalUser(username)) => {
            format!(
                "This device already has a different local account named '{username}'. Remove that local account from Settings → Account, then try pairing again."
            )
            .into()
        }
    }
}

pub(super) async fn pair_device(attempt: &PairAttempt) -> Result<PreparedPairing, PairDeviceError> {
    let status_run = pairing_sync_status::begin_run();
    let result = pair_device_inner(attempt, status_run).await;
    if let Err(error) = &result {
        pairing_sync_status::clear(status_run);
        if let Some(reason) = error.rejection_kind() {
            e2e::event!("pairing_rejected", reason = %reason);
        }
    }
    result
}

async fn pair_device_inner(
    attempt: &PairAttempt,
    status_run: u64,
) -> Result<PreparedPairing, PairDeviceError> {
    let (dht_key, keypair, account_id, password_hash) =
        decrypt_pairing_payload(&attempt.payload, &attempt.pin, &attempt.password).await?;
    let pairing_id = pairing_beacon::pairing_id_for_payload(&attempt.payload)
        .map_err(|e| PairDeviceError::DeviceLogin(NewDeviceLoginError::Internal(e)))?;
    let run = PairingRun {
        status_run,
        pairing_id,
    };
    let (user, dht_key, app_state, identity_docs) =
        user::new_device_login(dht_key, keypair, account_id, password_hash, run).await?;
    Ok(PreparedPairing {
        user,
        dht_key,
        app_state,
        identity_docs,
        run,
    })
}
