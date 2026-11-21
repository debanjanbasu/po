//! Schema-version validation for Cap'n Proto snapshots.
//!
//! Stored rows are strict about compatibility: matching versions decode
//! normally, older versions are reported to the caller's replace policy, and
//! newer versions are never discarded or downgraded.

use anyhow::Result;
use thiserror::Error;

/// Typed mismatch between a stored schema version and the running one.
#[derive(Debug, Error)]
pub enum SchemaVersionError {
    /// Stored data predates the running schema. Callers implementing
    /// "replace old data" semantics match this variant.
    #[error("{type_name}: stored schema v{stored} predates running schema v{current}")]
    Older {
        type_name: &'static str,
        stored: u32,
        current: u32,
    },
    /// Stored data was written by a newer build. Never auto-discarded.
    #[error("{type_name}: unsupported schema v{stored} (running code understands v{current})")]
    Newer {
        type_name: &'static str,
        stored: u32,
        current: u32,
    },
}

impl SchemaVersionError {
    /// True when the stored bytes are from a schema this build has moved past.
    ///
    /// Cap'n Proto fills fields added since with schema defaults, so such bytes
    /// still decode. Whether decoding them is *safe* is per-store policy — the
    /// meaning of unchanged fields may have shifted — so this predicate only
    /// identifies the case; each reader decides.
    #[must_use]
    pub const fn is_older(&self) -> bool {
        matches!(self, Self::Older { .. })
    }
}

/// Unpacks versioned bytes and returns the value and its schema version.
///
/// Cap'n Proto readers accept older payloads (missing fields decode as
/// defaults), so the embedded version is observable before compatibility is
/// checked.
pub fn unpack_versioned<T>(
    type_name: &'static str,
    current_version: u32,
    bytes: &[u8],
    unpack: impl Fn(&[u8]) -> Result<(T, u32)>,
) -> Result<(T, u32)> {
    let (value, version) = unpack(bytes)?;
    if version == current_version {
        return Ok((value, version));
    }
    Err(if version > current_version {
        SchemaVersionError::Newer {
            type_name,
            stored: version,
            current: current_version,
        }
    } else {
        SchemaVersionError::Older {
            type_name,
            stored: version,
            current: current_version,
        }
    }
    .into())
}
