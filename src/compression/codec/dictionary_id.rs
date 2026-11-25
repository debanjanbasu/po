//! Dictionary identities and structured-frame headers.

use std::{
    fmt::{self, Display, Formatter},
    mem::size_of,
    str::FromStr,
};

use anyhow::{Context, Error as AnyhowError, Result, bail};
use thiserror::Error;

use crate::helpers::encryption::crypto_system_sync;

pub const VERSION_HEADER_BYTES: usize = size_of::<u64>();
const CONTENT_ID_MARKER: u64 = u64::MAX;
const HASH_BYTES: usize = 32;

/// Dictionary identities are plain framing or a full VLD0/BLAKE3 content digest.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DictionaryId {
    #[default]
    Plain,
    Content([u8; HASH_BYTES]),
}

impl DictionaryId {
    pub(super) fn for_dictionary(dict: &[u8]) -> Result<Self> {
        let digest = crypto_system_sync()?.generate_hash(dict);
        Ok(Self::Content(
            digest
                .value()
                .bytes()
                .as_ref()
                .try_into()
                .context("dictionary digest is not 32 bytes")?,
        ))
    }

    pub(crate) fn append_header(self, out: &mut Vec<u8>) {
        match self {
            Self::Plain => out.extend_from_slice(&0_u64.to_le_bytes()),
            Self::Content(hash) => {
                out.extend_from_slice(&CONTENT_ID_MARKER.to_le_bytes());
                out.extend_from_slice(&hash);
            }
        }
    }

    pub(super) const fn header_len(self) -> usize {
        match self {
            Self::Content(_) => VERSION_HEADER_BYTES + HASH_BYTES,
            Self::Plain => VERSION_HEADER_BYTES,
        }
    }

    pub(crate) fn read_header(data: &[u8]) -> Result<(Self, &[u8])> {
        let header = data
            .get(..VERSION_HEADER_BYTES)
            .context("missing compression dictionary header")?;
        let version = u64::from_le_bytes(header.try_into()?);
        let payload = &data[VERSION_HEADER_BYTES..];
        match version {
            0 => Ok((Self::Plain, payload)),
            CONTENT_ID_MARKER => {
                let hash = payload
                    .get(..HASH_BYTES)
                    .context("truncated compression dictionary content identity")?;
                Ok((Self::Content(hash.try_into()?), &payload[HASH_BYTES..]))
            }
            _ => bail!("numeric dictionary frames are not supported"),
        }
    }
}

impl Display for DictionaryId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain => f.write_str("0"),
            Self::Content(hash) => {
                f.write_str("b3:")?;
                for byte in hash {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    }
}

impl FromStr for DictionaryId {
    type Err = AnyhowError;

    fn from_str(value: &str) -> Result<Self> {
        if let Some(hash) = value.strip_prefix("b3:") {
            if hash.len() != HASH_BYTES * 2 {
                bail!("invalid dictionary content identity length");
            }
            let mut bytes = [0; HASH_BYTES];
            hex::decode_to_slice(hash, &mut bytes)
                .context("invalid dictionary content identity")?;
            return Ok(Self::Content(bytes));
        }
        match value.parse::<u64>()? {
            0 => Ok(Self::Plain),
            CONTENT_ID_MARKER => bail!("dictionary content identity is missing its digest"),
            _ => bail!("numeric dictionary identities are not supported"),
        }
    }
}

#[derive(Debug, Error)]
pub enum DictionaryError {
    #[error("missing structured dictionary {0}")]
    Missing(DictionaryId),
    #[error("dictionary identity {0} refers to different bytes; refusing to replace it")]
    Collision(DictionaryId),
}
