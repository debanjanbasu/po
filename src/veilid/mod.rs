//! Veilid integration layer: core lifecycle, table store, DHT store, and sync.
//! See also: docs/dht-architecture.md, docs/watch-system.md.

pub mod change_dispatcher;
pub mod config;
pub mod core;
pub mod dict_sync;
pub mod network;
pub mod store;
pub mod table_store;
