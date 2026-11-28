//! Traits for models that sync to DHT via automerge CRDT.
//!
//! [`SyncSurface`] is the single abstraction every synced model implements:
//! metadata and plain subkey read/write.
//! [`CrdtModel`] extends it with a Cap'n Proto snapshot codec for the
//! dual-format local tables (snapshot col 0 + automerge col 1) — child
//! records and `AppState`. See also: docs/dht-architecture.md,
//! docs/sync-pipeline.md.

use anyhow::Result;
use autosurgeon::{Hydrate, Reconcile};
use dioxus::prelude::*;
use veilid_core::bytes::Bytes as VeilidBytes;

use crate::{models::pending_operation::OpType, user::ctx::UserCtx};

mod child_sync;
mod dual_row;
mod identity_doc;

pub use child_sync::pull_and_merge;
pub(super) use dual_row::load_user_retention_policy;
pub use dual_row::{
    COL_AUTOMERGE, COL_SNAPSHOT, DUAL_FORMAT_TABLE_COLS, LiveDoc, PullOutcome, ReservedMeta,
    automerge_marker_key, compaction_watermark_entry, decode_snapshot, delete_row, live_doc,
    load_automerge_current_marker, load_local, locked_live_doc, merge_remote, persist_written_doc,
    reconcile_snapshot_sidecar, replace_local, save_local, seed_local_doc,
};
pub use identity_doc::{
    IDENTITY_DOC_TABLE, load_doc_bytes, merge_remote_doc, reconcile_local_doc, store_doc_bytes,
};

/// Subscribe a local-data resource to committed changes for its model.
pub fn use_model_revision<T: CrdtModel>() -> Memo<u64> {
    use_memo(|| *T::revision_signal().read())
}

/// Notify after a successful snapshot commit, including atomic bulk writers.
pub fn notify_model_changed<T: CrdtModel>() {
    T::revision_signal().with_mut(|r| *r = r.wrapping_add(1));
}

// ── SyncSurface ─────────────────────────────────────────────────────────

/// The single abstraction for anything synced to the DHT as an automerge
/// CRDT document.
///
/// A new synced model implements this once and gets plain subkey reads/
/// writes, local-doc plumbing ([`load_doc_bytes`]/[`store_doc_bytes`]),
/// remote merging ([`merge_remote_doc`]), and a slot in the unified publish
/// path. Surfaces that additionally want dual-format local storage (fast
/// Cap'n Proto snapshot + automerge sidecar) implement [`CrdtModel`], which
/// adds only the snapshot codec — every sync semantic lives here.
pub trait SyncSurface: Default + Clone + Send + Reconcile + Hydrate + 'static {
    /// Unique identifier. Child records: `"totp"`, `"contacts"`,
    /// `"conversations"` (also the feature-pointer id and table-name
    /// suffix). Identity subkeys: `"profile"`, `"security"`, `"app_state"`.
    const ID: &'static str;

    /// Human-readable name for logs.
    const DISPLAY_NAME: &'static str;

    /// Local `TableDB` table holding the surface's documents when placed in a
    /// dual-format table (`"po_<ID>"`). Unused for pure identity-subkey surfaces.
    const TABLE_NAME: &'static str = "";

    /// DHT subkey within the target record — the identity-subkey index for
    /// identity-record surfaces, almost always 0 for child records.
    const SUBKEY: u32 = 0;

    /// Expected subkeys when creating a child DHT record.
    const CHILD_SUBKEYS: u16 = 1;
}

// ── CrdtModel ───────────────────────────────────────────────────────────

/// A [`SyncSurface`] persisted locally with dual-format storage.
///
/// **Read path**: Cap'n Proto snapshot (col 0) — zero-copy, fast.
/// **Write path**: Cap'n Proto snapshot (col 0) only.
/// **Sync path**: automerge doc (col 1) reconciled on-demand and marked current by hashing the authoritative snapshot bytes.
/// **Merge path**: automerge `doc.merge()` (col 1) — true CRDT semantics.
/// **DHT path**: automerge `reconcile_write` — loads existing, reconciles, stores.
pub trait CrdtModel: SyncSurface {
    /// Pending operation type for queue-based sync.
    const OP_TYPE: OpType;

    /// Current Cap'n Proto schema version for this model.
    const SCHEMA_VERSION: u32;

    /// Per-model reactive revision counter incremented on every local commit.
    fn revision_signal() -> &'static GlobalSignal<u64>;

    /// Serialize to Cap'n Proto packed bytes for fast local storage.
    fn pack(&self) -> Result<Vec<u8>>;

    /// Deserialize from Cap'n Proto packed bytes.
    /// Returns `(data, stored_schema_version)`.
    fn unpack(bytes: &[u8]) -> Result<(Self, u32)>;
    /// Save locally and enqueue for background DHT sync.
    async fn persist_and_sync(&self, ctx: &UserCtx) -> Result<()> {
        save_local::<Self>(ctx.storage_key(), self).await?;
        ctx.enqueue(Self::OP_TYPE, VeilidBytes::new()).await
    }
}

/// Implements the full sync surface for a dual-format model: the
/// [`SyncSurface`] metadata impl plus the [`CrdtModel`] snapshot codec.
macro_rules! impl_crdt_model {
    (
        $ty:ty,
        id = $id:literal,
        display = $display:literal,
        op = $op:expr,
        schema = $schema:expr,
        pack = $pack:path,
        unpack = $unpack:path
        $(, placement = $_placement:ident)?
        $(, subkey = $subkey:expr)?
        $(,)?
    ) => {
        impl $crate::storage::store_trait::SyncSurface for $ty {
            const ID: &'static str = $id;
            const DISPLAY_NAME: &'static str = $display;
            const TABLE_NAME: &'static str = concat!("po_", $id);
            $(const SUBKEY: u32 = $subkey;)?
        }

        impl $crate::storage::store_trait::CrdtModel for $ty {
            const OP_TYPE: OpType = $op;
            const SCHEMA_VERSION: u32 = $schema;
            fn revision_signal() -> &'static dioxus::prelude::GlobalSignal<u64> {
                static REVISION: dioxus::prelude::GlobalSignal<u64> =
                    dioxus::prelude::Signal::global(|| 0);
                &REVISION
            }

            fn pack(&self) -> anyhow::Result<Vec<u8>> {
                $pack(self)
            }

            fn unpack(bytes: &[u8]) -> anyhow::Result<(Self, u32)> {
                $unpack(bytes)
            }
        }
    };
}

pub(crate) use impl_crdt_model;
