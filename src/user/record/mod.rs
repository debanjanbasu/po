//! Identity record DHT operations.
//!
//! Manages automerge documents for CRDT-based conflict-free merging across
//! devices. Identity records use DFLT(16) with profile/security/app-state,
//! encrypted keypair blob, feature pointers, and routing/compression metadata.
//!
//! Owns: this directory — see [docs/dht-architecture.md](../../../docs/dht-architecture.md).

mod child;
mod create;
mod publish;
mod reencrypt;
mod sync;

pub use child::{child_owner_keypair, resolve_child_key_for_id};
pub use create::create_identity_record;
pub use reencrypt::reencrypt_subkeys;
pub(super) use sync::repull_until_onboarded;
pub use sync::{IdentityPull, RecordUnreachable, sync_from_dht, sync_from_dht_for_pairing};
