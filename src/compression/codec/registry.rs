//! Versioned dictionary registry: in-memory dictionaries, activation, and eviction.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroU32,
    sync::{Arc, LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use anyhow::{Result, bail};
use bytes::Bytes;
use tokio::sync::{Mutex as TokioMutex, OnceCell};
use tracing::warn;
use zstd::zstd_safe::get_dict_id_from_dict;

use crate::{
    compression::recompression,
    config::{compression, e2e},
    helpers::{encryption::get_cached_crypto_system, store_lock::recover_poison},
    veilid::table_store,
};

use super::{DictionaryError, DictionaryId, codec_pools};

/// Embedded dictionary bytes — loaded at startup on all platforms.
const STRUCTURED_ZDICT: &[u8] = include_bytes!("../../../assets/dict/structured.zdict");

/// Pre-parsed zstd dictionary metadata alongside raw dictionary bytes.
pub(in crate::compression) struct PreparedDict {
    bytes: Arc<[u8]>,
    dict_id: Option<NonZeroU32>,
}

impl PreparedDict {
    pub(super) fn new(bytes: Arc<[u8]>) -> Self {
        let dict_id = get_dict_id_from_dict(&bytes);
        Self { bytes, dict_id }
    }

    pub(in crate::compression) const fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }

    pub(in crate::compression) const fn dict_id(&self) -> Option<NonZeroU32> {
        self.dict_id
    }
}

// ── Dictionary registry (versioned) ──────────────────────────────────────────

/// Global dictionary registry with versioning support.
static REGISTRY: LazyLock<RwLock<DictionaryRegistry>> =
    LazyLock::new(|| RwLock::new(DictionaryRegistry::default()));

fn read_registry() -> RwLockReadGuard<'static, DictionaryRegistry> {
    recover_poison(REGISTRY.read())
}

fn write_registry() -> RwLockWriteGuard<'static, DictionaryRegistry> {
    recover_poison(REGISTRY.write())
}

/// Registry holding multiple dictionary versions.
#[derive(Default)]
pub struct DictionaryRegistry {
    current_version: DictionaryId,
    dicts: HashMap<DictionaryId, Arc<PreparedDict>>,
    insertion_order: VecDeque<DictionaryId>,
}

impl DictionaryRegistry {
    fn add_version(&mut self, version: DictionaryId, dict: Arc<[u8]>) -> Result<()> {
        if let Some(existing) = self.dicts.get(&version) {
            if existing.bytes.as_ref() != dict.as_ref() {
                return Err(DictionaryError::Collision(version).into());
            }
            return Ok(());
        }
        let prepared = Arc::new(PreparedDict::new(dict));
        self.dicts.insert(version, prepared);
        self.insertion_order.push_back(version);
        while self.dicts.len() > compression::MAX_DICT_VERSIONS {
            let Some(oldest) = self
                .insertion_order
                .iter()
                .find(|&&candidate| candidate != version && candidate != self.current_version)
                .copied()
            else {
                break;
            };
            warn!(
                "Evicting dictionary v{oldest} (max {} versions)",
                compression::MAX_DICT_VERSIONS
            );
            self.dicts.remove(&oldest);
            self.insertion_order.retain(|&id| id != oldest);
        }
        Ok(())
    }

    fn version_keys(&self) -> Vec<DictionaryId> {
        self.dicts.keys().copied().collect()
    }
}

pub(super) fn structured_snapshot() -> Result<(DictionaryId, Arc<PreparedDict>)> {
    let registry = read_registry();
    let version = registry.current_version;
    let dict = registry
        .dicts
        .get(&version)
        .cloned()
        .ok_or(DictionaryError::Missing(version))?;
    drop(registry);
    Ok((version, dict))
}

fn prune_pool_versions(versions: &[DictionaryId]) {
    codec_pools().prune_structured_versions(versions);
}

// ── Dictionary registry operations ───────────────────────────────────────────

/// Current dictionary version. Cold path: reads the registry lock; there is
/// no separate atomic mirror of this value.
pub fn current_version() -> DictionaryId {
    read_registry().current_version
}

/// Caches a dictionary version in memory only; persistence is managed separately.
pub(super) fn cache_dict_version(version: DictionaryId, dict: Arc<[u8]>) -> Result<()> {
    let versions = {
        let mut registry = write_registry();
        registry.add_version(version, dict)?;
        registry.version_keys()
    };
    prune_pool_versions(&versions);
    Ok(())
}

/// Returns the dictionary for the given version as a cheap `Arc` clone — no byte copying.
pub fn get_dict_arc(version: DictionaryId) -> Option<Arc<[u8]>> {
    read_registry()
        .dicts
        .get(&version)
        .map(|d| Arc::clone(d.bytes()))
}

pub(in crate::compression) fn get_prepared_dict(
    version: DictionaryId,
) -> Option<Arc<PreparedDict>> {
    read_registry().dicts.get(&version).cloned()
}

pub(super) fn activate_dict_version_in_memory(
    version: DictionaryId,
    dict: Arc<[u8]>,
) -> Result<()> {
    let versions = {
        let mut registry = write_registry();
        registry.add_version(version, dict)?;
        registry.current_version = version;
        registry.version_keys()
    };
    prune_pool_versions(&versions);
    Ok(())
}

// ── Immutable dictionary persistence, lazy loading, and activation ──────────

const ACTIVE_DICTIONARY_KEY: &[u8] = b"active_dictionary";

static DICTIONARY_UPDATE_LOCK: TokioMutex<()> = TokioMutex::const_new(());
static DICTIONARIES_READY: OnceCell<()> = OnceCell::const_new();

async fn validate_dictionary(version: DictionaryId, dict: &[u8]) -> Result<()> {
    if version == DictionaryId::Plain {
        bail!("the dictionary-free codec cannot have a dictionary");
    }
    drop(get_cached_crypto_system().await?);
    if DictionaryId::for_dictionary(dict)? != version {
        return Err(DictionaryError::Collision(version).into());
    }
    Ok(())
}

/// Load an immutable dictionary and keep its bytes alive across an async codec
/// dispatch, even if another task evicts its hot registry entry.
pub async fn load_dict_version(version: DictionaryId) -> Result<Option<Arc<[u8]>>> {
    reload_from_tablestore().await?;
    load_dict_version_inner(version).await
}

async fn load_dict_version_inner(version: DictionaryId) -> Result<Option<Arc<[u8]>>> {
    Ok(load_prepared_dict_version(version)
        .await?
        .map(|prepared| Arc::clone(prepared.bytes())))
}

pub(in crate::compression) async fn load_prepared_dict_version(
    version: DictionaryId,
) -> Result<Option<Arc<PreparedDict>>> {
    if let Some(prepared) = get_prepared_dict(version) {
        return Ok(Some(prepared));
    }
    let key = format!("structured.v{version}");
    let Some(dict) = table_store::load_dict_opt(key.as_bytes()).await? else {
        return Ok(None);
    };
    validate_dictionary(version, &dict).await?;
    let dict: Arc<[u8]> = Arc::from(dict);
    cache_dict_version(version, dict)?;
    Ok(get_prepared_dict(version))
}

pub(in crate::compression) async fn load_frame_dictionary(
    data: &[u8],
) -> Result<Option<(DictionaryId, Arc<PreparedDict>)>> {
    let (version, _) = DictionaryId::read_header(data)?;
    if version == DictionaryId::Plain {
        return Ok(None);
    }
    reload_from_tablestore().await?;
    Ok(Some((
        version,
        load_prepared_dict_version(version)
            .await?
            .ok_or(DictionaryError::Missing(version))?,
    )))
}

/// Initialize the embedded dictionary by content identity, then restore the active identity.
pub async fn reload_from_tablestore() -> Result<()> {
    DICTIONARIES_READY
        .get_or_try_init(reload_dictionaries)
        .await?;
    Ok(())
}

async fn reload_dictionaries() -> Result<()> {
    let _guard = DICTIONARY_UPDATE_LOCK.lock().await;
    drop(get_cached_crypto_system().await?);
    let embedded = DictionaryId::for_dictionary(STRUCTURED_ZDICT)?;
    if get_dict_arc(embedded).is_none() {
        persist_dict_version_locked(embedded, STRUCTURED_ZDICT.to_vec()).await?;
    }
    let current = match table_store::load_dict_opt(ACTIVE_DICTIONARY_KEY).await? {
        Some(bytes) => {
            let version: DictionaryId = String::from_utf8(bytes)?.trim().parse()?;
            if !matches!(version, DictionaryId::Content(_)) {
                bail!("active dictionary must have a content identity");
            }
            version
        }
        None => embedded,
    };
    let dict = load_dict_version_inner(current)
        .await?
        .ok_or(DictionaryError::Missing(current))?;
    activate_cached_version_locked(current, dict).await
}

/// Install a remote content-identified dictionary and activate it atomically.
pub async fn install_and_activate_dict_version(version: DictionaryId, dict: Vec<u8>) -> Result<()> {
    reload_from_tablestore().await?;
    let _guard = DICTIONARY_UPDATE_LOCK.lock().await;
    install_and_activate_locked(dict, Some(version)).await
}

/// Harness seam: this device's active dictionary and the network's announced
/// dictionary are the same content, so both sides converged on one digest.
/// The digest is the app's own content identity (`b3:<digest>`) — never a
/// per-device numeric version. A dictionary-free (`Plain`) frame has no
/// content digest, so it is never reported.
pub fn report_dict_converged(version: DictionaryId) {
    if !matches!(version, DictionaryId::Content(_)) {
        return;
    }
    e2e::event!("dict_synced", digest = %version);
}

/// Deletes stored dictionary blobs no local row is framed with any more.
///
/// `observed` is the set of frame versions a *completed* recompression scan
/// found across every structured table, which is the evidence that a version
/// is unreferenced: after that scan no local row is framed with it, and the
/// rows that referenced it were rewritten with the current dictionary.
/// `keep` also always includes the registry's live versions — the same set the
/// codec pools are pruned to — the active version, and the build's embedded
/// dictionary, whose blob is re-stored at every startup otherwise.
///
/// Runs under the dictionary-update lock, so a dictionary installed
/// concurrently with the prune (a sibling device's dictionary arriving through
/// `dict_sync`) cannot have its blob deleted between its `store_dict` and its
/// activation. What it cannot cover is a *remote* copy still framed with a
/// pruned version — a subkey an older build wrote and nobody has republished
/// since; see the recompression call site for that trade.
pub(in crate::compression) async fn prune_unreferenced_dicts(
    observed: &HashSet<DictionaryId>,
) -> Result<usize> {
    let _guard = DICTIONARY_UPDATE_LOCK.lock().await;
    let mut keep: Vec<String> = read_registry()
        .dicts
        .keys()
        .copied()
        .map(|version| version.to_string())
        .collect();
    if let Ok(embedded) = DictionaryId::for_dictionary(STRUCTURED_ZDICT) {
        keep.push(embedded.to_string());
    }
    keep.extend(observed.iter().map(DictionaryId::to_string));
    table_store::prune_dict_versions(&keep).await
}

async fn persist_dict_version_locked(version: DictionaryId, dict: Vec<u8>) -> Result<()> {
    validate_dictionary(version, &dict).await?;
    if let Some(existing) = get_dict_arc(version)
        && existing.as_ref() != dict.as_slice()
    {
        return Err(DictionaryError::Collision(version).into());
    }

    let key = format!("structured.v{version}");
    if let Some(existing) = table_store::load_dict_opt(key.as_bytes()).await? {
        if existing != dict {
            return Err(DictionaryError::Collision(version).into());
        }
        cache_dict_version(version, Arc::from(dict))?;
        return Ok(());
    }
    let dict_arc: Arc<[u8]> = Arc::from(dict);
    table_store::store_dict(key.as_bytes(), Bytes::from_owner(Arc::clone(&dict_arc))).await?;
    cache_dict_version(version, dict_arc)?;
    Ok(())
}

/// Activate a dictionary version as current, persisting to `TableDB` and notifying recompression.
pub async fn activate_dict_version(version: DictionaryId) -> Result<()> {
    reload_from_tablestore().await?;
    let _guard = DICTIONARY_UPDATE_LOCK.lock().await;
    let dict = load_dict_version_inner(version)
        .await?
        .ok_or(DictionaryError::Missing(version))?;
    if !matches!(version, DictionaryId::Content(_)) {
        bail!("active dictionary must have a content identity");
    }
    activate_cached_version_locked(version, dict).await
}

async fn activate_cached_version_locked(version: DictionaryId, dict: Arc<[u8]>) -> Result<()> {
    table_store::store_dict(ACTIVE_DICTIONARY_KEY, version.to_string()).await?;
    activate_dict_version_in_memory(version, dict)?;
    recompression::notify_recompress();
    Ok(())
}

async fn install_and_activate_locked(dict: Vec<u8>, expected: Option<DictionaryId>) -> Result<()> {
    let version = DictionaryId::for_dictionary(&dict)?;
    if let Some(expected) = expected
        && expected != version
    {
        return Err(DictionaryError::Collision(expected).into());
    }
    persist_dict_version_locked(version, dict).await?;
    let dict = get_dict_arc(version).ok_or(DictionaryError::Missing(version))?;
    activate_cached_version_locked(version, dict).await
}

/// Allocate by content, serialize activation, and persist before the in-memory swap.
pub(in crate::compression) async fn swap_dict(dict: Vec<u8>) -> Result<()> {
    reload_from_tablestore().await?;
    let _guard = DICTIONARY_UPDATE_LOCK.lock().await;
    drop(get_cached_crypto_system().await?);
    install_and_activate_locked(dict, None).await
}
