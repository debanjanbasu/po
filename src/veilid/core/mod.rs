//! Veilid API lifecycle: singleton init, update processing.

mod updates;

use std::{
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, OnceCell};
use tracing::warn;
#[cfg(not(target_arch = "wasm32"))]
use veilid_core::VeilidAPIError::NotInitialized;
use veilid_core::{
    RecordKey, VeilidAPI, VeilidAPIResult, VeilidAppLifecycleState, VeilidConfig,
    VeilidStateAttachmentCondition, api_startup,
};

// Capabilities this build declines to advertise. Only the mobile targets set
// any, so the import is gated with them.
#[cfg(any(target_os = "ios", target_os = "android"))]
use veilid_core::{VEILID_CAPABILITY_RELAY, VEILID_CAPABILITY_VALIDATE_DIAL_INFO};

#[cfg(not(target_arch = "wasm32"))]
use crate::veilid::config;
#[cfg(not(target_arch = "wasm32"))]
use crate::veilid::store::clear_pins;
use crate::{
    config::{app::APP_ID, e2e},
    helpers::{async_utils::now_epoch_millis, spawn_blocking},
    sync::queue as sync_queue,
    veilid::change_dispatcher,
};

use updates::{clear_update_ingress, install_update_ingress, update_callback};

/// Lock-free API singleton (mirrors `CRYPTO_API` in `helpers::encryption`).
/// Initialized once via `ensure_local_api()`; `veilid_shutdown()` is terminal —
/// there is no in-process restart path.
static VEILID_API: OnceCell<VeilidAPI> = OnceCell::const_new();

/// Whether `attach()` has been called successfully on the current API instance.
static NETWORK_ATTACHED: AtomicBool = AtomicBool::new(false);

/// Serializes concurrent `attach_network()` callers so `attach()` and the
/// sync-queue startup run at most once — a check-then-act race on
/// `NETWORK_ATTACHED` alone would double-attach.
static ATTACH_GATE: Mutex<()> = Mutex::const_new(());

fn create_config() -> VeilidConfig {
    #[cfg(not(target_arch = "wasm32"))]
    let storage_directory = Some(config::get_base_dir());
    #[cfg(target_arch = "wasm32")]
    let storage_directory: Option<String> = None;

    let mut config = VeilidConfig::new(
        "po",
        "debanjanbasu",
        "com",
        storage_directory.as_deref(),
        None,
    );
    config.namespace = e2e::instance().map_or_else(
        || APP_ID.to_string(),
        |instance| format!("{APP_ID}-{instance}"),
    );
    // Seam builds take the same path as debug ones, and for the same reason:
    // the real protected store can raise an interactive keychain prompt, which
    // would stall an unattended harness run. `e2e-seams` is never enabled for a
    // distributed build, so nothing shipped is affected.
    #[cfg(any(debug_assertions, feature = "e2e-seams"))]
    {
        config.protected_store.always_use_insecure_storage = true;
    }
    // Deliberately above core's own default (`VeilidConfigDHT::default()`):
    // the adaptive DHT byte gate is off in every build, so this cap is the
    // only governor on
    // in-flight outbound DHT fanout. What it bounds is peak concurrent RPCs —
    // their sockets and buffers — and the raise trades that headroom for
    // fewer operations queued behind a cold routing table. It is not a
    // throughput target.
    config.network.dht.max_concurrent_operations = if cfg!(target_arch = "wasm32") {
        64
    } else {
        512
    };
    // Capabilities this app never serves. It runs no tunnels, answers no
    // AppCall and stores no blocks, and `RELAY` and `VALIDATE_DIAL_INFO` are
    // pure service to other nodes: peers may route through this device, or dial
    // it to validate their own reachability. On a metered radio that is inbound
    // work the app gets nothing back from, so both are dropped wherever the
    // radio is metered. Desktop and web keep the full set — nothing there is
    // metered, and advertising the connectivity capabilities is what keeps a
    // node where peers look for one.
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        config.capabilities.disable = vec![
            VEILID_CAPABILITY_RELAY,
            VEILID_CAPABILITY_VALIDATE_DIAL_INFO,
        ];
    }
    config
}

/// Initializes the Veilid API locally (`api_startup` only, no network attachment).
///
/// This provides access to `table_store()`, `crypto()`, and `protected_store()`
/// without requiring any network connectivity. Idempotent — subsequent calls
/// return the cached handle.
pub async fn ensure_local_api() -> VeilidAPIResult<VeilidAPI> {
    // Fast path: lock-free read once initialized.
    if let Some(api) = VEILID_API.get() {
        return Ok(api.clone());
    }

    let api = VEILID_API
        .get_or_try_init(|| async {
            change_dispatcher::init();
            install_update_ingress();

            // `create_config` prunes stale sibling data dirs with sync `std::fs`
            // (native); keep that off the async worker thread. Best-effort, so a
            // failed blocking task falls back to running it inline.
            let config = spawn_blocking(create_config)
                .await
                .unwrap_or_else(|_| create_config());
            match api_startup(Arc::new(update_callback), config).await {
                Ok(api) => Ok(api),
                Err(e) => {
                    clear_update_ingress();
                    Err(e)
                }
            }
        })
        .await?;
    Ok(api.clone())
}

/// Attaches to the Veilid network and starts the background sync queue.
///
/// Requires `ensure_local_api()` to have succeeded first. Idempotent — returns
/// immediately if already attached.
pub async fn attach_network() -> VeilidAPIResult<()> {
    if NETWORK_ATTACHED.load(Ordering::Acquire) {
        return Ok(());
    }

    let _gate = ATTACH_GATE.lock().await;
    // Re-check under the gate: a concurrent caller may have finished the
    // attach while this one waited.
    if NETWORK_ATTACHED.load(Ordering::Acquire) {
        return Ok(());
    }

    let api = ensure_local_api().await?;

    api.attach().await?;

    sync_queue::get_sync_queue();
    sync_queue::start_background_processor();

    NETWORK_ATTACHED.store(true, Ordering::Release);
    Ok(())
}

/// Waits (bounded) until the node can serve a DHT record operation.
///
/// Asks Veilid directly, through its attachment condition wait on
/// `public_internet_ready` — the level-based signal the storage manager gates
/// record opens on, so this returns exactly when an open will be accepted.
/// Waiting on plain connectivity is not enough, and was the bug: seconds after
/// attach a node has live peers while the storage manager still refuses the
/// open with `TryAgain`, so the caller reported a failure indistinguishable
/// from a missing record.
///
/// Returns `true` if readiness was reached within `budget_ms`. On timeout or
/// API shutdown it returns `false`; callers report a network error rather than
/// a false "record not found".
pub async fn wait_for_dht_readiness(budget_ms: u64) -> bool {
    let started = now_epoch_millis();
    let condition = VeilidStateAttachmentCondition {
        opt_public_internet_ready: Some(true),
        ..Default::default()
    };
    let Ok(api) = get_veilid_api().await else {
        warn!("DHT readiness wait skipped: Veilid API unavailable");
        return false;
    };
    let result = api
        .wait_for_attachment_condition(Some(condition), Some(Duration::from_millis(budget_ms)))
        .await;
    let waited_ms = now_epoch_millis().saturating_sub(started);
    match result {
        Ok(_) => {
            // Harness seam: proves the caller waited on Veilid's own DHT
            // gate rather than on a weaker signal, and says how long.
            e2e::event!("dht_ready", waited_ms = waited_ms);
            true
        }
        Err(error) => {
            warn!("DHT not ready after {waited_ms}ms: {error}");
            e2e::event!("dht_ready_timeout", budget_ms = budget_ms);
            false
        }
    }
}

/// Returns a fully-initialized Veilid API with network attached.
///
/// Use this for operations that require network connectivity (DHT, routing
/// context, private routes, `app_message`/`app_call`). For local-only operations
/// (`TableDB`, `Crypto`), prefer `ensure_local_api()`.
pub async fn get_veilid_api() -> VeilidAPIResult<VeilidAPI> {
    let api = ensure_local_api().await?;
    if !NETWORK_ATTACHED.load(Ordering::Acquire) {
        attach_network().await?;
    }
    Ok(api)
}

/// Shuts down the Veilid API. Terminal — the process must exit afterwards;
/// there is no in-process restart path. Native only: web has no shutdown
/// edge, so nothing calls it there.
#[cfg(not(target_arch = "wasm32"))]
pub async fn veilid_shutdown() -> VeilidAPIResult<()> {
    NETWORK_ATTACHED.store(false, Ordering::Release);
    clear_update_ingress();

    if let Some(api) = VEILID_API.get() {
        // API teardown closes every record; pin state must not survive it.
        clear_pins();
        api.clone().shutdown().await;
        Ok(())
    } else {
        Err(NotInitialized)
    }
}

/// Notifies Veilid of a host app foreground/background transition when the API
/// has been initialized. No-op before `ensure_local_api()` completes.
pub fn report_lifecycle_state(state: VeilidAppLifecycleState) {
    if let Some(api) = VEILID_API.get()
        && let Err(e) = api.report_app_lifecycle_state(state)
    {
        warn!("Veilid lifecycle state report failed: {e}");
    }
}
/// Parse a base64 DHT record key string with a descriptive error.
///
/// Also validates the opaque key length: veilid's `RecordKey::from_str`
/// accepts any base64 payload — including the empty string, which decodes
/// to a zero-length key that only fails later at `open_dht_record` with
/// "invalid record key length: 0 != 32". Rejecting it here keeps
/// "unparseable" and "unusable" the same thing for callers (e.g. the
/// identity watcher treats a parse failure as "no identity record yet").
pub fn parse_record_key(input: &str) -> Result<RecordKey> {
    /// Opaque record key length in bytes; mirrors veilid-core's internal
    /// `HASH_COORDINATE_LENGTH` (not re-exported), enforced by
    /// `check_record_key` on every record open.
    const RECORD_KEY_LENGTH: usize = 32;

    let key = RecordKey::from_str(input).with_context(|| format!("Invalid record key: {input}"))?;
    let len = key.ref_value().ref_key().len();
    if len != RECORD_KEY_LENGTH {
        bail!("Invalid record key (opaque length {len} != {RECORD_KEY_LENGTH}): {input:?}");
    }
    Ok(key)
}
