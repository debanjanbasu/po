//! Shared length-prefixed binary framing.
//!
//! Every length-prefixed payload in the app's wire/backup formats goes
//! through this module, the one place that writes and bounds-checks those
//! prefixes, so the copies cannot drift apart on bounds checks:
//!
//! - `chat/signal/wire.rs` — `PeerSignal` sender-proof public key and
//!   signature and the `SyncNudge` device id, both `u16`.
//! - `chat/signal/routes/routes_dht.rs` — route-entry blobs: device id (`u16`),
//!   blob (`u32`), and the leading entry-count field (`u32`, written as a
//!   4-byte fixed payload via `push_u32_len_prefixed`).
//! - `sync/pending_ops/mod.rs` — dedup lock-key target/peer/manifest strings
//!   (`u32` via `push_str`).
//! - `storage/backup.rs` — identity strings, kv-list keys/data and count,
//!   payload section data and count (`u32`).
//!
//! Bare fixed-width fields (timestamps, version u32s, raw counts read as
//! plain u32, record-key kinds) are not length-prefixed and stay raw.
//!
//! All readers are bounds-checked against untrusted bytes: a declared length
//! that runs past the buffer is an error, never a slice panic. `string_slice`
//! is denied crate-wide, so slicing goes through `get(..)`.

use anyhow::{Context, Result, bail};

/// Appends `value` with a `u16` little-endian length prefix.
pub fn push_u16_len_prefixed(out: &mut Vec<u8>, value: &[u8]) {
    let len = u16::try_from(value.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value);
}

/// Appends `value` with a `u32` little-endian length prefix.
pub fn push_u32_len_prefixed(out: &mut Vec<u8>, value: &[u8]) {
    let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value);
}

/// Appends a string with a `u32` little-endian length prefix (the backup
/// format's `encode_str`).
pub fn push_str(out: &mut Vec<u8>, value: &str) {
    push_u32_len_prefixed(out, value.as_bytes());
}

/// Reads a `u16`-prefixed payload at `bytes[offset..]`, returning the payload
/// and the offset just past it. Errors when the prefix or the declared length
/// runs past the buffer.
pub fn read_u16_len_prefixed<'a>(
    bytes: &'a [u8],
    offset: usize,
    what: &str,
) -> Result<(&'a [u8], usize)> {
    let len_bytes = bytes
        .get(offset..offset + 2)
        .context(format!("{what} length bytes"))?;
    let len = usize::from(u16::from_le_bytes([len_bytes[0], len_bytes[1]]));
    let start = offset + 2;
    let end = start + len;
    let payload = bytes
        .get(start..end)
        .with_context(|| format!("{what} payload: {end} > {}", bytes.len()))?;
    Ok((payload, end))
}

/// Reads a `u32`-prefixed payload at `bytes[offset..]`, returning the payload
/// and the offset just past it.
pub fn read_u32_len_prefixed<'a>(
    bytes: &'a [u8],
    offset: usize,
    what: &str,
) -> Result<(&'a [u8], usize)> {
    let len_bytes = bytes
        .get(offset..offset + 4)
        .context(format!("{what} length bytes"))?;
    let len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    let start = offset + 4;
    let end = start + len;
    if end < start {
        bail!("{what} length overflows: {len}");
    }
    let payload = bytes
        .get(start..end)
        .with_context(|| format!("{what} payload: {end} > {}", bytes.len()))?;
    Ok((payload, end))
}
