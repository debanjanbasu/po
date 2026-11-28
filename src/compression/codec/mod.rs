//! Shared compression utilities for Veilid storage.
//!
//! Local rows use content-identified learned dictionaries; DHT records use
//! dictionary-free version-zero frames. Dictionary blobs themselves use raw
//! plain zstd so bootstrap never depends on the dictionary being loaded.
//!
mod dictionary_id;
mod registry;

use std::{
    collections::HashMap,
    io::{Cursor, Read},
    sync::{Arc, LazyLock, Mutex, RwLock},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::Bytes;
use zstd::{
    bulk::{Compressor, Decompressor},
    stream::read::Decoder,
    zstd_safe::{compress_bound, get_dict_id_from_frame, get_frame_content_size},
};

use crate::{
    config,
    helpers::{spawn_blocking, store_lock::recover_poison},
};

pub use crate::config::compression::ZSTD_LEVEL;
pub use dictionary_id::{DictionaryError, DictionaryId};
use registry::structured_snapshot;
pub(in crate::compression) use registry::{
    PreparedDict, get_prepared_dict, load_frame_dictionary, prune_unreferenced_dicts, swap_dict,
};
pub use registry::{
    activate_dict_version, current_version, get_dict_arc, install_and_activate_dict_version,
    load_dict_version, reload_from_tablestore, report_dict_converged,
};

// ── Pooled zstd compressor/decompressor contexts ────────────────────────────

const MAX_IDLE_CONTEXTS_PER_POOL: usize = 4;

pub(super) struct ContextPool<T> {
    items: Mutex<Vec<T>>,
}

impl<T> Default for ContextPool<T> {
    fn default() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }
}

impl<T> ContextPool<T> {
    pub(super) fn take(&self) -> Option<T> {
        recover_poison(self.items.lock()).pop()
    }

    pub(super) fn put(&self, value: T) {
        let mut items = recover_poison(self.items.lock());
        if items.len() < MAX_IDLE_CONTEXTS_PER_POOL {
            items.push(value);
        }
    }
}

#[derive(Default)]
pub(in crate::compression) struct CodecPools {
    compressors: RwLock<HashMap<DictionaryId, Arc<ContextPool<Compressor<'static>>>>>,
    decompressors: RwLock<HashMap<DictionaryId, Arc<ContextPool<Decompressor<'static>>>>>,
}

fn pool<T>(
    pools: &RwLock<HashMap<DictionaryId, Arc<ContextPool<T>>>>,
    key: DictionaryId,
) -> Arc<ContextPool<T>> {
    let existing = recover_poison(pools.read()).get(&key).cloned();
    if let Some(pool) = existing {
        return pool;
    }
    recover_poison(pools.write())
        .entry(key)
        .or_insert_with(|| Arc::new(ContextPool::default()))
        .clone()
}

impl CodecPools {
    pub(super) fn compressor_pool(
        &self,
        key: DictionaryId,
    ) -> Arc<ContextPool<Compressor<'static>>> {
        pool(&self.compressors, key)
    }

    pub(super) fn decompressor_pool(
        &self,
        key: DictionaryId,
    ) -> Arc<ContextPool<Decompressor<'static>>> {
        pool(&self.decompressors, key)
    }

    pub(super) fn prune_structured_versions(&self, valid_versions: &[DictionaryId]) {
        recover_poison(self.compressors.write())
            .retain(|key, _| *key == DictionaryId::Plain || valid_versions.contains(key));
        recover_poison(self.decompressors.write())
            .retain(|key, _| *key == DictionaryId::Plain || valid_versions.contains(key));
    }
}

static CODEC_POOLS: LazyLock<CodecPools> = LazyLock::new(CodecPools::default);

pub(in crate::compression) fn codec_pools() -> &'static CodecPools {
    &CODEC_POOLS
}

// ── Synchronous zstd kernels ────────────────────────────────────────────────

pub(in crate::compression) fn frame_version(data: &[u8]) -> Result<DictionaryId> {
    DictionaryId::read_header(data).map(|(version, _)| version)
}

pub(in crate::compression) fn compress_structured_blocking(data: &[u8]) -> Result<Vec<u8>> {
    let (version, dict) = structured_snapshot()?;
    compress_framed(data, version, Some(&dict))
}

/// Stable transport framing: version zero followed by dictionary-free zstd.
/// Peer-shared and password-private DHT records both use this codec so retained
/// network history never depends on a device's learned dictionary window.
pub fn compress_transport_blocking(data: &[u8]) -> Result<Vec<u8>> {
    compress_framed(data, DictionaryId::Plain, None)
}

fn compress_framed(
    data: &[u8],
    version: DictionaryId,
    dict: Option<&PreparedDict>,
) -> Result<Vec<u8>> {
    with_pooled_compressor(
        version,
        || {
            dict.map_or_else(
                || Compressor::new(ZSTD_LEVEL),
                |prepared| Compressor::with_dictionary(ZSTD_LEVEL, prepared.bytes()),
            )
            .context("failed to create framed compressor")
        },
        |compressor| {
            // Build [version_header | compressed_payload] in a single allocation.
            // NOTE: `Cursor<Vec<u8>>` is required because zstd-safe's `WriteBuf`
            // impl for bare `Vec<u8>` writes from byte 0 and calls `set_len(n)`
            // (absolute), clobbering any pre-existing content. The `Cursor` impl
            // correctly offsets writes by its position.
            let bound = compress_bound(data.len());
            let mut output = Vec::with_capacity(version.header_len() + bound);
            version.append_header(&mut output);
            let mut cursor = Cursor::new(output);
            cursor.set_position(u64::try_from(version.header_len())?);
            compressor
                .compress_to_buffer(data, &mut cursor)
                .context("compression failed")?;
            Ok(cursor.into_inner())
        },
    )
}

fn compress_plain_blocking(data: &[u8]) -> Result<Vec<u8>> {
    with_pooled_compressor(
        DictionaryId::Plain,
        || Compressor::new(ZSTD_LEVEL).context("failed to create plain compressor"),
        |compressor| compressor.compress(data).context("compression failed"),
    )
}

/// Runs `f` with a compressor drawn from the pool for `key`, creating it via
/// `create` on pool miss and returning it to the pool afterwards — including
/// on error.
fn with_pooled_compressor<T>(
    key: DictionaryId,
    create: impl FnOnce() -> Result<Compressor<'static>>,
    f: impl FnOnce(&mut Compressor<'static>) -> Result<T>,
) -> Result<T> {
    let pool = codec_pools().compressor_pool(key);
    let mut compressor = match pool.take() {
        Some(compressor) => compressor,
        None => create()?,
    };
    let result = f(&mut compressor);
    pool.put(compressor);
    result
}

pub(in crate::compression) fn decompress_structured_with_dictionary(
    data: &[u8],
    dict: Option<(DictionaryId, &PreparedDict)>,
) -> Result<Vec<u8>> {
    let (version, payload) = DictionaryId::read_header(data)?;
    if payload.is_empty() {
        bail!("structured compression header is missing its zstd frame");
    }
    if version == DictionaryId::Plain {
        return decompress_plain_payload_blocking(payload);
    }
    let (dictionary_id, prepared) = dict.ok_or(DictionaryError::Missing(version))?;
    if version != dictionary_id {
        return Err(DictionaryError::Collision(version).into());
    }
    if get_dict_id_from_frame(payload) != prepared.dict_id() {
        bail!("zstd frame does not match structured dictionary {version}");
    }

    decompress_with_pool(
        dictionary_id,
        || {
            Decompressor::with_dictionary(prepared.bytes())
                .context("failed to create structured decompressor")
        },
        payload,
        Some(prepared.bytes()),
        config::compression::MAX_DECOMPRESSED_BYTES,
    )
}

fn decompress_plain_payload_blocking(data: &[u8]) -> Result<Vec<u8>> {
    decompress_plain_payload_blocking_bounded(data, config::compression::MAX_DECOMPRESSED_BYTES)
}

pub(in crate::compression) fn decompress_plain_payload_blocking_bounded(
    data: &[u8],
    max_output_bytes: usize,
) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    decompress_with_pool(
        DictionaryId::Plain,
        || Decompressor::new().context("failed to create plain decompressor"),
        data,
        None,
        max_output_bytes,
    )
}

fn decompress_with_pool(
    key: DictionaryId,
    create: impl FnOnce() -> Result<Decompressor<'static>>,
    data: &[u8],
    dictionary: Option<&[u8]>,
    max_output_bytes: usize,
) -> Result<Vec<u8>> {
    let frame_size = get_frame_content_size(data)
        .ok()
        .flatten()
        .map(usize::try_from)
        .transpose()?;
    let Some(frame_size) = frame_size else {
        let read_limit = u64::try_from(max_output_bytes)?
            .checked_add(1)
            .context("decompression limit is too large")?;
        let decoder = Decoder::with_dictionary(data, dictionary.unwrap_or_default())
            .context("failed to create streaming decompressor")?
            // Match bulk decompression: decode one frame, not concatenated frames.
            .single_frame();
        let mut decompressed = Vec::new();
        decoder
            .take(read_limit)
            .read_to_end(&mut decompressed)
            .map_err(|e| anyhow!("decompression failed: {e}"))?;
        ensure!(
            decompressed.len() <= max_output_bytes,
            "decompressed payload exceeds {max_output_bytes} byte limit"
        );
        return Ok(decompressed);
    };
    ensure!(
        frame_size <= max_output_bytes,
        "decompressed payload exceeds {max_output_bytes} byte limit"
    );
    let pool = codec_pools().decompressor_pool(key);
    let mut decompressor = match pool.take() {
        Some(decompressor) => decompressor,
        None => create()?,
    };

    let decompressed = decompressor
        .decompress(data, frame_size)
        .map_err(|e| anyhow!("decompression failed: {e}"));
    pool.put(decompressor);
    decompressed
}

// ── Async compression/decompression entry points ────────────────────────────

pub async fn compress_structured(data: impl Into<Bytes>) -> Result<Bytes> {
    reload_from_tablestore().await?;
    run_codec(data.into(), compress_structured_blocking).await
}

pub async fn compress_plain(data: impl Into<Bytes>) -> Result<Bytes> {
    run_codec(data.into(), compress_plain_blocking).await
}

pub async fn decompress_structured(data: impl Into<Bytes>) -> Result<Bytes> {
    let data = data.into();
    if data.is_empty() {
        return Ok(Bytes::new());
    }
    let dict = load_frame_dictionary(&data).await?;
    run_codec(data, move |d| {
        decompress_structured_with_dictionary(
            d,
            dict.as_ref().map(|(id, prepared)| (*id, prepared.as_ref())),
        )
    })
    .await
}

pub async fn decompress_plain(data: impl Into<Bytes>) -> Result<Bytes> {
    let data = data.into();
    if data.is_empty() {
        return Ok(Bytes::new());
    }
    run_codec(data, decompress_plain_payload_blocking).await
}

/// Decompresses a plain zstd payload without allowing output above the given limit.
pub async fn decompress_plain_limited(
    data: impl Into<Bytes>,
    max_output_bytes: usize,
) -> Result<Bytes> {
    let data = data.into();
    if data.is_empty() {
        return Ok(Bytes::new());
    }
    run_codec(data, move |d| {
        decompress_plain_payload_blocking_bounded(d, max_output_bytes)
    })
    .await
}

/// Run a blocking codec operation on a background thread.
async fn run_codec(
    data: Bytes,
    f: impl FnOnce(&[u8]) -> Result<Vec<u8>> + Send + 'static,
) -> Result<Bytes> {
    Ok(Bytes::from(spawn_blocking(move || f(&data)).await??))
}
