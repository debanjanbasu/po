//! Table store persistence for pending operations and dead-letter queue.

use anyhow::Result;
use futures_util::future::join_all;

use crate::{
    models::pending_operation::{PendingOperation, UnsupportedOpRow},
    storage::background_errors::report_nonfatal,
    veilid::table_store::{self, Table},
};

use super::{
    COL_DEAD_LETTER, COL_PENDING, PENDING_OPS_COLS, PENDING_OPS_TABLE, column_name, id_to_key,
    indexes::with_indexes_mut, is_completion_marker, is_row_cleanup_marker,
};

async fn discard_corrupt_op(
    col: u32,
    id: u64,
    expected: &[u8],
    context: &'static str,
    detail: String,
) -> Result<()> {
    let table = table_store::Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS).await?;
    let key = id_to_key(id);
    if !table.delete_if_matches(col, &key, expected).await? {
        return Ok(());
    }
    report_nonfatal(
        "sync.pending_ops",
        context,
        format!("{} id={id}: {detail}", column_name(col)),
    );
    with_indexes_mut(|idx| {
        if col == COL_PENDING {
            idx.remove_pending_and_dedup(id, None);
        } else {
            idx.dismiss_dead_letter(id);
        }
    })
    .await;
    Ok(())
}

pub(super) async fn decode_row_for_id(
    col: u32,
    id: u64,
    bytes: &[u8],
) -> Result<Option<PendingOperation>> {
    match PendingOperation::from_bytes(bytes) {
        Ok(op) if op.id == id => Ok(Some(op)),
        Ok(op) => {
            discard_corrupt_op(
                col,
                id,
                bytes,
                "Deleting op with mismatched key id",
                format!("stored id={}", op.id),
            )
            .await?;
            Ok(None)
        }
        // A row another build understands — a schema version this one does not
        // run, or an `OpType` variant it does not know — stays on disk and is
        // skipped like any other unreadable row: one such row must not abort
        // the whole listing, park the drain for every account, or fail the
        // startup rebuild behind it. Deleting it would discard durable work
        // the writing build may still execute.
        Err(e) if e.downcast_ref::<UnsupportedOpRow>().is_some() => {
            report_nonfatal(
                "sync.pending_ops",
                "Skipping queue row this build cannot use",
                format!("{} id={id}: {e:#}", column_name(col)),
            );
            Ok(None)
        }
        Err(e) => {
            discard_corrupt_op(col, id, bytes, "Deleting corrupt op", e.to_string()).await?;
            Ok(None)
        }
    }
}

async fn decode_op_opt(col: u32, id: u64, bytes: &[u8]) -> Result<Option<PendingOperation>> {
    Ok(decode_row_for_id(col, id, bytes)
        .await?
        .filter(|op| !is_completion_marker(op) && !is_row_cleanup_marker(op)))
}

pub(super) async fn load_op_opt(col: u32, id: u64) -> Result<Option<PendingOperation>> {
    let key = id_to_key(id);
    let Some(bytes) = Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS)
        .await?
        .load_opt(col, &key)
        .await?
    else {
        return Ok(None);
    };
    decode_op_opt(col, id, &bytes).await
}

pub(super) async fn store_op(col: u32, op: &PendingOperation) -> Result<()> {
    Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS)
        .await?
        .store(col, &id_to_key(op.id), &op.to_bytes()?)
        .await
}

/// Commits the destination row and retires its predecessors atomically.
pub(super) async fn store_op_replacing(
    column: u32,
    op: &PendingOperation,
    removed: &[(u32, u64)],
) -> Result<()> {
    if removed.is_empty() {
        return store_op(column, op).await;
    }
    let keys: Vec<_> = removed
        .iter()
        .map(|&(col, id)| (col, id_to_key(id)))
        .collect();
    let deletes: Vec<_> = keys
        .iter()
        .map(|(col, key)| (*col, key.as_slice()))
        .collect();
    let key = id_to_key(op.id);
    let bytes = op.to_bytes()?;
    table_store::Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS)
        .await?
        .apply_batch(&deletes, &[(column, &key, &bytes)])
        .await
}

pub(super) async fn delete_both_rows(id: u64) -> Result<()> {
    let key = id_to_key(id);
    table_store::Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS)
        .await?
        .apply_batch(&[(COL_PENDING, &key), (COL_DEAD_LETTER, &key)], &[])
        .await
}

pub(super) async fn delete_op(col: u32, id: u64) -> Result<()> {
    Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS)
        .await?
        .delete(col, &id_to_key(id))
        .await
}

/// Bounds concurrent queue-row reads per batch in listings and startup rebuilds.
const LOAD_OPS_CHUNK: usize = 32;

pub(super) async fn load_decoded_rows_by_ids(
    table: &table_store::Table,
    ids: &[u64],
    col: u32,
) -> Result<Vec<(u64, PendingOperation)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    // Parallel fetch phase: issue DB reads concurrently in bounded,
    // order-preserving chunks. Table is Arc-backed, so cloning is a
    // pointer bump.
    let mut read_results = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(LOAD_OPS_CHUNK) {
        let chunk_results = join_all(chunk.iter().map(|&id| {
            let table = table.clone();
            async move { (id, table.load_opt(col, &id_to_key(id)).await) }
        }))
        .await;
        read_results.extend(chunk_results);
    }

    // Sequential processing phase: deserialize results and handle errors
    // (error handling has side effects: delete + index mutation).
    let mut ops = Vec::with_capacity(ids.len());
    for (id, read_result) in read_results {
        // A transient single-row read failure must not stall the whole drain.
        let bytes = match read_result {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(error) => {
                report_nonfatal(
                    "sync.pending_ops",
                    "Skipping unreadable queue row",
                    format!("id={id}: {error:#}"),
                );
                continue;
            }
        };
        if let Some(op) = decode_row_for_id(col, id, &bytes).await? {
            ops.push((id, op));
        }
    }
    Ok(ops)
}

pub(super) async fn load_ops_by_ids(ids: &[u64], col: u32) -> Result<Vec<PendingOperation>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let table = table_store::Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS).await?;
    Ok(load_decoded_rows_by_ids(&table, ids, col)
        .await?
        .into_iter()
        .map(|(_, op)| op)
        .filter(|op| !is_completion_marker(op) && !is_row_cleanup_marker(op))
        .collect())
}
