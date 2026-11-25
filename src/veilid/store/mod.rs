//! Generic DHT record service: CRUD over Veilid DHT with built-in zstd
//! compression and AEAD encryption. Callers pass raw bytes; compression and
//! encryption are handled transparently.
//!
//! Owns: `src/veilid/store/` — encoding ([`mode`]), record lifecycle +
//! scope locks ([`lifecycle`]), pins, watches, transactions, and the public
//! subkey API. See docs/dht-architecture.md.

mod api;
mod lifecycle;
mod mode;
mod pushed;
mod reconcile;
mod transaction;

// Read surface: `load_opt_with_mode` (private, in `mode`) is the single read
// implementation; `load_opt`, `load_structured`, `load_unencrypted`, and
// `read_subkey` are thin aliases over it, and `read_subkey_raw` shares its
// fetch (`api::fetch_subkey_raw`) while returning bytes verbatim.
pub use api::{
    DhtReadError, DhtSubkeyMissing, inspect_record, is_transient_network_error, load_opt,
    load_structured, load_unencrypted, read_subkey, read_subkey_raw,
    report_nonfatal_unless_network, store, write_subkey_raw,
};
#[cfg(not(target_arch = "wasm32"))]
pub use lifecycle::clear_pins;
pub use lifecycle::{
    WatchError, derive_record_key, ensure_record_with_keypair, flush_dht_record, pin_record,
    record_owner, record_schema, unpin_record, watch_record, with_record, with_record_timeout,
};
pub use mode::{StoreMode, decode_with_mode, store_with_mode};
pub use pushed::{PushedValue, load_opt_preferring_push, pushed_value};
pub use reconcile::{ReconcileWriteError, reconcile_write, reconcile_write_open};
pub use transaction::{DhtTx, with_transaction};
