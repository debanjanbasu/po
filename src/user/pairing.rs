//! Cross-device pairing admission: staged device-login effects, supersession guards, and post-admission hydration.
use super::{
    User,
    auth::{
        RememberedSessionEffect, apply_remembered_session_effect, build_remembered_session_effect,
        encrypt_keypair, take_matching_staged,
    },
    identity_doc, load_user, store_user,
};
use crate::{
    chat::record as chat_record,
    config::pairing::{DHT_READY_BUDGET_MS, JOIN_COMPLETE_LINGER_SECS},
    helpers::{
        async_utils::sleep_ms,
        capnp::capnp_u32,
        encryption::{crypto_system, derive_dht_secret_from_hash},
        spawn,
        store_lock::{lock_store, recover_poison},
    },
    models::app_state::AppState,
    nonfatal,
    session::epoch::{SessionGuard, spawn_session_detached},
    storage::{
        background_errors::report_nonfatal,
        hydration_status::{
            clear_hydration_status, increment_hydration_completed, increment_hydration_failed,
            set_hydration_started,
        },
        pairing_sync_status::{self, PairingSyncStage},
        store_trait::{live_doc, save_local},
    },
    user::{
        CURRENT_CRYPTO_VERSION,
        ctx::UserCtx,
        identity_layout,
        pairing_beacon::{self, BeaconMilestone, PairingId},
        password_rotation::{identity_sync_lock, validate_rotation_secret},
        record as identity_record,
        store::load_matching_user,
        sync_models::{SyncPointers, SyncProfile, SyncSecurity},
        watcher::{
            HydrationBurstGuard, begin_hydration_burst, child_pull::pull_identity_children,
            prepare_pull,
        },
    },
    veilid::{
        core::{ensure_local_api, wait_for_dht_readiness},
        dict_sync,
    },
};
use anyhow::{Context, Error, Result, anyhow, ensure};
use bytes::Bytes;
use std::sync::{Arc, LazyLock, Mutex};
use thiserror::Error as ThisError;
use veilid_core::{DHTSchema, KeyPair, RecordKey, SharedSecret, VeilidAPIError};
use zeroize::Zeroizing;

#[derive(Debug, ThisError)]
pub enum NewDeviceLoginError {
    #[error(
        "A different local account already uses username '{0}'. Remove that local account before pairing this identity on this device."
    )]
    ConflictingLocalUser(String),
    #[error(
        "Couldn't reach this account's sync record on the network. Check this device's connection, keep the account's other device online, and try again."
    )]
    RecordUnreachable(#[source] Error),
    #[error(
        "This device couldn't get ready on the sync network in time. Check its internet connection and try again."
    )]
    NetworkNotReady(#[source] Error),
    #[error(
        "Your other device hasn't finished publishing this account's data yet. Keep that device open and online, then try again."
    )]
    SourceStale(#[source] Error),
    #[error(transparent)]
    Internal(#[from] Error),
}

/// Correlates one pairing attempt across status tracking and the beacon.
///
/// `status_run` guards local [`pairing_sync_status`] stage writes against a
/// superseding attempt; `pairing_id` ties beacon milestones to the exact QR
/// the sharing device is displaying.
#[derive(Debug, Clone, Copy)]
pub struct PairingRun {
    pub status_run: u64,
    pub pairing_id: PairingId,
}

/// Remote identity history staged until the matching pairing admission commits.
pub struct IdentitySeedDocs {
    pub profile: Bytes,
    pub security: Bytes,
    pub app_state: Bytes,
}

struct PendingDeviceLogin {
    username: Arc<str>,
    admission_id: u64,
    dht_key: RecordKey,
    app_state: AppState,
    identity_docs: IdentitySeedDocs,
    remember_effect: RememberedSessionEffect,
    run: PairingRun,
}

static PENDING_DEVICE_LOGIN: LazyLock<Mutex<Option<PendingDeviceLogin>>> =
    LazyLock::new(|| Mutex::new(None));

async fn build_local_user(
    dht_key: &RecordKey,
    keypair: &KeyPair,
    account_id: &str,
    dht_secret: &SharedSecret,
    profile: SyncProfile,
    security: SyncSecurity,
    pointers: SyncPointers,
) -> Result<User> {
    let encrypted_keypair = encrypt_keypair(keypair, dht_secret).await?;
    let mut user = User::new(
        profile.username,
        dht_key.to_string(),
        account_id,
        encrypted_keypair,
        keypair.clone(),
    );
    user.set_dob(profile.dob);
    user.set_terms_accepted(profile.terms_accepted);
    user.set_pin_hash(security.pin_hash);
    user.set_biometrics_enabled(security.biometrics_enabled);
    user.runtime.sync_pointers = Some(pointers);
    Ok(user)
}

/// First beacon milestone, written before the identity fetch (the slowest
/// pairing step — minutes on a cold routing table): tells the sharing device
/// its QR was scanned and this device is joining. Spawned so it never delays
/// the fetch; the QR payload already carries the owner keypair needed to
/// write. Scope-owned: if the user abandons pairing, the write dies with the
/// pairing screen, and the share side's TTL handles staleness.
fn spawn_accepted_beacon(
    dht_key: RecordKey,
    keypair: KeyPair,
    dht_secret: SharedSecret,
    pairing_id: PairingId,
) {
    spawn(async move {
        nonfatal!(
            "user.pairing",
            "Failed to write pairing beacon (accepted)",
            pairing_beacon::write_milestone(
                &dht_key,
                &keypair,
                &dht_secret,
                pairing_id,
                BeaconMilestone::Accepted,
            )
            .await
        );
    });
}

fn post_pairing_hydration(account_id: Arc<str>, dht_key: RecordKey, run: PairingRun) {
    // Detached: survives the pairing screen unmounting (navigation to the
    // main app), but cancels on logout. Logout's UI cleanup clears both
    // hydration and pairing-sync statuses, so a cancelled run leaves nothing
    // stuck (see `reset_to_anonymous`). Captures only owned data and
    // global statics — no component-owned signals.
    spawn_session_detached(async move {
        let Some((ctx, guard)) = UserCtx::try_current_with_guard() else {
            pairing_sync_status::clear(run.status_run);
            return;
        };
        if ctx.account_id != account_id {
            pairing_sync_status::clear(run.status_run);
            return;
        }
        let Some((identity, crypto_lease)) = prepare_pull(&ctx, guard).await else {
            // Pending rotations resume through the startup-resync latch.
            pairing_sync_status::clear(run.status_run);
            return;
        };
        // Own the identity pull for the burst's duration: the inspect poll
        // and connectivity edge hold their pending-latch Full resyncs until
        // this guard drops instead of duplicating these child-record pulls.
        let _hydration_burst: HydrationBurstGuard = begin_hydration_burst();

        nonfatal!(
            "user.pairing",
            "Failed to write pairing beacon (syncing)",
            pairing_beacon::write_milestone(
                &dht_key,
                &identity.keypair,
                &identity.secret,
                run.pairing_id,
                BeaconMilestone::Syncing,
            )
            .await
        );

        pairing_sync_status::set_stage(run.status_run, PairingSyncStage::ChildRecords);
        pull_identity_children(&ctx, &dht_key, &identity.secret, &identity.keypair, guard).await;

        pairing_sync_status::set_stage(run.status_run, PairingSyncStage::Conversations);
        let conversations_hydrated = hydrate_paired_conversations(&ctx, guard).await;

        pairing_sync_status::set_stage(run.status_run, PairingSyncStage::Dictionary);
        nonfatal!(
            "user.pairing",
            "Dict sync check failed after pairing",
            dict_sync::check_and_sync(dht_key.clone(), &identity.secret).await
        );

        if !conversations_hydrated {
            // History could not be loaded: say so instead of showing a green
            // "complete" over an empty conversation list. The failure itself
            // is already in the Activity panel; the beacon stays at `Syncing`
            // so the sharing device's UI does not claim the sync finished,
            // and the sync queue can still catch the index up once it is
            // readable again.
            pairing_sync_status::set_stage(run.status_run, PairingSyncStage::Failed);
            drop(crypto_lease);
            pairing_sync_status::clear(run.status_run);
            return;
        }

        pairing_sync_status::set_stage(run.status_run, PairingSyncStage::Complete);
        // Harness seam: the completion milestone has no prose log line, and
        // native surfaces have no DOM to scrape.
        crate::config::e2e::event!("paired");
        nonfatal!(
            "user.pairing",
            "Failed to write pairing beacon (complete)",
            pairing_beacon::write_milestone(
                &dht_key,
                &identity.keypair,
                &identity.secret,
                run.pairing_id,
                BeaconMilestone::Complete,
            )
            .await
        );
        drop(crypto_lease);

        // Leave the completion state visible briefly, then reset.
        sleep_ms(JOIN_COMPLETE_LINGER_SECS * 1_000).await;
        pairing_sync_status::clear(run.status_run);
    });
}

/// Eagerly hydrates all conversations for the paired identity — pulling key
/// rings, manifests, rosters, and message history from the DHT before the old
/// device goes offline — while tracking hydration progress and persisting any
/// conversations that failed to hydrate.
///
/// Returns `false` when the run must not reach `PairingSyncStage::Complete`:
/// the conversation index could not be read, so this device hydrated nothing
/// and the user has to learn that instead of seeing "sync complete" over an
/// empty history.
async fn hydrate_paired_conversations(ctx: &UserCtx, guard: SessionGuard) -> bool {
    let failed = chat_record::hydrate_all_conversations(
        ctx,
        guard,
        |total| {
            guard.commit(|| set_hydration_started(capnp_u32(total)));
        },
        |success| {
            guard.commit(|| {
                if success {
                    increment_hydration_completed();
                } else {
                    increment_hydration_failed();
                }
            });
        },
    )
    .await;
    guard.commit(clear_hydration_status);
    if !failed.is_empty()
        && let Err(e) = chat_record::store_failed_hydrations(ctx, guard, &failed).await
    {
        report_nonfatal(
            "user.pairing",
            "Failed to persist incomplete hydration list",
            e,
        );
    }
    // Hydrations that failed individually (and were persisted for retry) do
    // not block `Complete` — the reconnect path owns them. A load failure is
    // different: nothing was attempted, so the run must stop before
    // `Complete`; `hydrate_all_conversations` reports it in the Activity
    // panel and returns the sentinel below instead of an empty list.
    !failed
        .iter()
        .any(|key| key.as_str() == chat_record::HYDRATION_LOAD_FAILED_KEY)
}

async fn validate_existing_paired_user(
    user: &User,
    dht_secret: &SharedSecret,
) -> Result<(), NewDeviceLoginError> {
    if let Some(existing) = load_user(user.username())
        .await
        .map_err(NewDeviceLoginError::from)?
    {
        if existing.dht_key() != user.dht_key() {
            return Err(NewDeviceLoginError::ConflictingLocalUser(
                user.username().to_owned(),
            ));
        }
        if existing.crypto_version() != CURRENT_CRYPTO_VERSION {
            return Err(anyhow!(
                "Unsupported local crypto version {}",
                existing.crypto_version()
            )
            .into());
        }
        if existing.account_id() != user.account_id() {
            return Err(NewDeviceLoginError::ConflictingLocalUser(
                user.username().to_owned(),
            ));
        }
        validate_rotation_secret(&existing, dht_secret).await?;
    }
    Ok(())
}

/// Prepares a local user from a pairing payload (DHT key, keypair, account ID, and password).
///
/// Validates record ownership before DHT access, then pulls the identity,
/// re-encrypts its keypair, and stages remote identity docs for admission.
pub async fn new_device_login(
    dht_key: RecordKey,
    keypair: KeyPair,
    account_id: String,
    password_hash: Zeroizing<String>,
    run: PairingRun,
) -> Result<(User, RecordKey, AppState, IdentitySeedDocs), NewDeviceLoginError> {
    crypto_system!(crypto);
    let keypair_valid = crypto
        .validate_keypair(&keypair.key(), &keypair.secret())
        .map_err(Error::from)?;
    if !keypair_valid {
        return Err(anyhow!("Pairing keypair is invalid").into());
    }
    let api = ensure_local_api().await.map_err(Error::from)?;
    let schema = DHTSchema::dflt(identity_layout::IDENTITY_SUBKEY_COUNT)
        .context("Failed to build identity schema")?;
    let derived_key = api
        .get_dht_record_key(schema, keypair.key(), None)
        .await
        .context("Failed to derive pairing identity key")?;
    if derived_key != dht_key {
        return Err(anyhow!("Pairing keypair does not own the supplied identity record").into());
    }
    let dht_secret = derive_dht_secret_from_hash(&password_hash, &account_id)
        .await
        .map_err(NewDeviceLoginError::from)?;
    // A fresh device often reaches this moments after Veilid starts, and a
    // DHT fanout from a cold routing table exhausts without finding replicas
    // — indistinguishable from a genuinely missing record. Hard gate: do not
    // read until the node is properly attached; on timeout fail with a
    // network error instead of a false "record not found".
    pairing_sync_status::set_stage(run.status_run, PairingSyncStage::Connecting);
    if !wait_for_dht_readiness(DHT_READY_BUDGET_MS).await {
        return Err(NewDeviceLoginError::NetworkNotReady(anyhow!(
            "sync network not ready within {}s",
            DHT_READY_BUDGET_MS / 1_000
        )));
    }
    spawn_accepted_beacon(
        dht_key.clone(),
        keypair.clone(),
        dht_secret.clone(),
        run.pairing_id,
    );
    pairing_sync_status::set_stage(run.status_run, PairingSyncStage::Identity);
    let mut pull = identity_record::sync_from_dht_for_pairing(&dht_key, &dht_secret)
        .await
        .map_err(|e| {
            if e.downcast_ref::<identity_record::RecordUnreachable>()
                .is_some()
            {
                NewDeviceLoginError::RecordUnreachable(e)
            } else if e.chain().any(|link| {
                link.downcast_ref::<VeilidAPIError>()
                    .is_some_and(|veilid| matches!(veilid, VeilidAPIError::TryAgain { .. }))
            }) {
                // A cold routing table or an unanswered fanout. That is a
                // network condition, not bad pairing data, so say so rather
                // than sending the user to re-check details that were fine.
                NewDeviceLoginError::NetworkNotReady(e)
            } else {
                NewDeviceLoginError::Internal(e)
            }
        })?;
    // The record was reachable but predates onboarding: the QR can only be
    // minted by an onboarded identity, so the sharing device's
    // post-onboarding publish is still in its sync queue (e.g. stuck on a
    // hostile network). Admitting this copy would pop the onboarding wizard
    // and fork history — re-pull until the complete copy lands or the
    // budget is spent.
    if !pull.is_onboarded() {
        pairing_sync_status::set_stage(run.status_run, PairingSyncStage::WaitingForSource);
        let Some(fresh) =
            identity_record::repull_until_onboarded(&dht_key, &dht_secret, pull).await
        else {
            return Err(NewDeviceLoginError::SourceStale(anyhow!(
                "identity record still missing onboarding data after re-poll budget"
            )));
        };
        pull = fresh;
    }
    pairing_sync_status::set_stage(run.status_run, PairingSyncStage::Admission);
    let identity_record::IdentityPull {
        profile,
        security,
        app_state,
        pointers,
        profile_doc,
        security_doc,
        app_state_doc,
    } = pull;
    let mut user = build_local_user(
        &dht_key,
        &keypair,
        &account_id,
        &dht_secret,
        profile,
        security,
        pointers,
    )
    .await
    .map_err(NewDeviceLoginError::from)?;

    validate_existing_paired_user(&user, &dht_secret).await?;

    let identity_docs = IdentitySeedDocs {
        profile: profile_doc,
        security: security_doc,
        app_state: app_state_doc,
    };

    user.runtime.dht_secret = Some(dht_secret);
    user.runtime.password_hash = Some(password_hash);

    Ok((user, dht_key, app_state, identity_docs))
}

/// Stages the local commit for a prepared paired user.
///
/// The user row, app-state snapshot, identity docs, remember-me persistence,
/// and post-pairing hydration kickoff wait for the matching authenticated admission.
pub fn stage_device_login(
    user: &mut User,
    dht_key: RecordKey,
    app_state: AppState,
    identity_docs: IdentitySeedDocs,
    remember_me: bool,
    run: PairingRun,
) {
    let admission_id = super::next_admission_id();
    let remember_effect =
        build_remembered_session_effect(user.runtime.password_hash.take(), remember_me);
    user.runtime.admission_id = Some(admission_id);
    *recover_poison(PENDING_DEVICE_LOGIN.lock()) = Some(PendingDeviceLogin {
        username: user.username_arc(),
        admission_id,
        dht_key,
        app_state,
        identity_docs,
        remember_effect,
        run,
    });
}

pub(super) fn clear_pending_device_login() {
    *recover_poison(PENDING_DEVICE_LOGIN.lock()) = None;
}

/// Applies a staged device-login commit for this admission attempt.
///
/// Returns `true` when a pending pairing admission was consumed — the caller
/// uses this to skip the redundant post-admission identity resync, since
/// pairing just pulled the identity record and `post_pairing_hydration`
/// covers child records, conversations, and dict sync.
pub(super) async fn apply_pending_device_login(user: &User, guard: &SessionGuard) -> Result<bool> {
    if !guard.is_current() {
        return Ok(false);
    }
    let Some(PendingDeviceLogin {
        admission_id,
        dht_key,
        app_state,
        identity_docs,
        remember_effect,
        run,
        ..
    }) = take_matching_staged(&PENDING_DEVICE_LOGIN, user, |p| {
        (&p.username, p.admission_id)
    })
    else {
        return Ok(false);
    };

    pairing_sync_status::set_stage(run.status_run, PairingSyncStage::AppState);
    if let Err(e) = store_paired_user(user, &app_state, &identity_docs, guard).await {
        if guard.is_current() {
            *recover_poison(PENDING_DEVICE_LOGIN.lock()) = Some(PendingDeviceLogin {
                username: user.username_arc(),
                admission_id,
                dht_key,
                app_state,
                identity_docs,
                remember_effect,
                run,
            });
        }
        return Err(e.context("Failed to store paired user locally"));
    }

    apply_remembered_session_effect(user, remember_effect, guard).await;
    if !guard.is_current() {
        return Ok(true);
    }
    post_pairing_hydration(user.account_id_arc(), dht_key, run);
    Ok(true)
}

async fn store_paired_user(
    user: &User,
    app_state: &AppState,
    identity_docs: &IdentitySeedDocs,
    guard: &SessionGuard,
) -> Result<()> {
    let _crypto_lease = identity_sync_lock(user.account_id()).write_owned().await;
    let _user_lock = lock_store(user.username().as_bytes()).await;
    if !guard.is_current() {
        return Ok(());
    }
    ensure!(
        user.crypto_version() == CURRENT_CRYPTO_VERSION,
        "Only current-crypto accounts may be committed from pairing"
    );
    if let Some(existing) = load_user(user.username()).await? {
        ensure!(
            existing.account_id() == user.account_id() && existing.dht_key() == user.dht_key(),
            "Pairing account was replaced while admission was pending"
        );
        ensure!(
            existing.crypto_version() == CURRENT_CRYPTO_VERSION,
            "Pairing requires a current-crypto account"
        );
        load_matching_user(user.username(), user.account_id())
            .await?
            .context("Pairing account is being deleted")?;
        validate_rotation_secret(
            &existing,
            user.runtime
                .dht_secret
                .as_ref()
                .context("Pairing credential is unavailable")?,
        )
        .await?;
    }
    {
        let _account_lock = lock_store(user.account_id().as_bytes()).await;
        if !guard.is_current() {
            return Ok(());
        }
        store_user(user).await?;
        if *app_state != AppState::default()
            && live_doc::<AppState>(user.account_id().as_bytes())
                .await?
                .is_none()
        {
            save_local::<AppState>(user.account_id().as_bytes(), app_state).await?;
        }
    }
    // Once this account-scoped commit starts, finish the staged causal-history
    // seed under the same username lock even if the UI epoch changes. The
    // account lock is released first: the seed merges the app-state doc
    // through `merge_remote`, which takes that lock itself (the store mutexes
    // are not reentrant).
    nonfatal!(
        "user.pairing",
        "Failed to seed local identity docs from pulled identity record",
        identity_doc::seed_docs(
            user.account_id(),
            identity_docs.profile.as_ref(),
            identity_docs.security.as_ref(),
            identity_docs.app_state.as_ref(),
        )
        .await
    );
    Ok(())
}
