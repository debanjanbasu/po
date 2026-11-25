//! Rebuilds in-memory queue indexes from a single table scan, pruning
//! shadowed/stale residue left by crashes between writes.

use std::{
    collections::{BTreeSet, HashMap},
    mem,
    sync::atomic::Ordering,
};

use anyhow::{Result, anyhow};

use crate::{
    models::pending_operation::{OpType, PendingOperation, seed_next_id},
    storage::{background_errors::report_nonfatal, sync_status::refresh_sync_status},
    veilid::table_store::{self, Table},
};

use super::{
    COL_DEAD_LETTER, COL_PENDING, PENDING_OPS_COLS, PENDING_OPS_TABLE, SEQ_COUNTER,
    SEQ_RESTART_GAP, id_to_key,
    indexes::{DedupKey, Indexes, dedup_key_for, indexes},
    is_completion_marker, is_row_cleanup_marker, key_to_id, row_cleanup_marker,
    storage::load_decoded_rows_by_ids,
};

struct QueuedRow {
    column: u32,
    id: u64,
    sequence_number: u64,
}

#[derive(Default)]
struct RebuildState {
    pending: HashMap<u64, PendingOperation>,
    dead_letter: HashMap<u64, PendingOperation>,
    stale_rows: HashMap<(u32, u64), StaleRow>,
    rewritten_rows: Vec<QueuedRow>,
    completion_marker_ids: BTreeSet<u64>,
    max_id: u64,
    max_seq: u64,
}

struct StaleRow {
    op: PendingOperation,
    context: &'static str,
}

impl RebuildState {
    fn record(&mut self, id: u64, op: &PendingOperation) {
        self.max_id = self.max_id.max(id);
        self.max_seq = self.max_seq.max(op.sequence_number);
    }

    fn mark_stale(&mut self, column: u32, op: PendingOperation, reason: &'static str) {
        self.stale_rows.insert(
            (column, op.id),
            StaleRow {
                op,
                context: reason,
            },
        );
    }
}

const fn row_rank(row: &QueuedRow) -> (u64, u8, u64) {
    (
        row.sequence_number,
        if row.column == COL_DEAD_LETTER { 1 } else { 0 },
        row.id,
    )
}

fn row_op<'a>(state: &'a RebuildState, row: &QueuedRow) -> Option<&'a PendingOperation> {
    if row.column == COL_PENDING {
        state.pending.get(&row.id)
    } else {
        state.dead_letter.get(&row.id)
    }
}

async fn scan_column(
    table: &table_store::Table,
    column: u32,
    state: &mut RebuildState,
    error_context: &'static str,
) -> Result<()> {
    let mut ids = Vec::new();
    for key in table.raw().get_keys(column).await? {
        let Some(id) = key_to_id(&key) else {
            Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS)
                .await?
                .delete(column, &key)
                .await?;
            continue;
        };
        state.max_id = state.max_id.max(id);
        ids.push(id);
    }

    for (id, op) in load_decoded_rows_by_ids(table, &ids, column).await? {
        state.record(id, &op);
        if is_completion_marker(&op) {
            state.completion_marker_ids.insert(id);
            state.mark_stale(column, op, error_context);
        } else if is_row_cleanup_marker(&op) {
            state.mark_stale(column, op, error_context);
        } else if column == COL_DEAD_LETTER {
            state.dead_letter.insert(id, op);
        } else {
            state.pending.insert(id, op);
        }
    }
    Ok(())
}

fn prune_shadowed_rebuild_rows(state: &mut RebuildState) {
    for id in mem::take(&mut state.completion_marker_ids) {
        if let Some(op) = state.pending.remove(&id) {
            state.mark_stale(
                COL_PENDING,
                op,
                "Removing pending row shadowed by completed-op marker during rebuild",
            );
        }
        if let Some(op) = state.dead_letter.remove(&id) {
            state.mark_stale(
                COL_DEAD_LETTER,
                op,
                "Removing dead-letter row shadowed by completed-op marker during rebuild",
            );
        }
    }

    let ids: Vec<_> = state.dead_letter.keys().copied().collect();
    for id in ids {
        if let Some(op) = state.pending.remove(&id) {
            state.mark_stale(
                COL_PENDING,
                op,
                "Removing pending row shadowed by dead-letter row during rebuild",
            );
        }
    }
}

fn prune_dedup_rebuild_rows(state: &mut RebuildState) {
    let mut dedup_groups = HashMap::<DedupKey, Vec<QueuedRow>>::new();
    for op in state.pending.values() {
        dedup_groups
            .entry(dedup_key_for(op))
            .or_default()
            .push(QueuedRow {
                column: COL_PENDING,
                id: op.id,
                sequence_number: op.sequence_number,
            });
    }
    for op in state.dead_letter.values() {
        dedup_groups
            .entry(dedup_key_for(op))
            .or_default()
            .push(QueuedRow {
                column: COL_DEAD_LETTER,
                id: op.id,
                sequence_number: op.sequence_number,
            });
    }

    for rows in dedup_groups.into_values() {
        if rows.len() < 2 {
            continue;
        }
        let winner = rows
            .iter()
            .max_by_key(|row| row_rank(row))
            .map(|row| (row.column, row.id));
        if let Some((column, id)) = winner {
            let winner_is_feed = if column == COL_PENDING {
                state.pending.get(&id)
            } else {
                state.dead_letter.get(&id)
            }
            .is_some_and(|op| op.op_type == OpType::ConvFeedSync);
            if winner_is_feed && rewrite_dedup_feed_payload(state, &rows, column, id) {
                continue;
            }
            for row in rows {
                if Some((row.column, row.id)) == winner {
                    continue;
                }
                let op = if row.column == COL_PENDING {
                    state.pending.remove(&row.id)
                } else {
                    state.dead_letter.remove(&row.id)
                };
                if let Some(op) = op {
                    state.mark_stale(
                        row.column,
                        op,
                        "Removing stale dedup duplicate during rebuild",
                    );
                }
            }
        }
    }
}

fn rewrite_dedup_feed_payload(
    state: &mut RebuildState,
    rows: &[QueuedRow],
    column: u32,
    id: u64,
) -> bool {
    let mut message_ids = Vec::new();
    for row in rows {
        let Some(op) = row_op(state, row) else {
            continue;
        };
        match op.conv_feed_message_ids() {
            Ok(ids) => message_ids.extend(ids),
            Err(error) => {
                report_nonfatal(
                    "sync.pending_ops",
                    "Preserving queue rows with corrupt feed message IDs",
                    format!("op={}: {error:#}", row.id),
                );
                return true;
            }
        }
    }
    let payload = PendingOperation::conv_feed_payload(message_ids);
    let rewritten_sequence = {
        let winner_op = if column == COL_PENDING {
            state.pending.get_mut(&id)
        } else {
            state.dead_letter.get_mut(&id)
        };
        if let Some(op) = winner_op
            && op.payload != payload
        {
            op.payload = payload;
            Some(op.sequence_number)
        } else {
            None
        }
    };
    if let Some(sequence_number) = rewritten_sequence {
        state.rewritten_rows.push(QueuedRow {
            column,
            id,
            sequence_number,
        });
    }
    false
}

fn build_rebuilt_indexes(state: &RebuildState) -> Indexes {
    let mut rebuilt = Indexes::default();

    for op in state.pending.values() {
        rebuilt.upsert_pending(op, None);
        rebuilt.set_dedup(op);
    }
    for op in state.dead_letter.values() {
        rebuilt.insert_dead_letter(op);
    }

    rebuilt
}

async fn cleanup_rebuild_rows(
    stale_rows: &HashMap<(u32, u64), StaleRow>,
    rewritten_rows: &[QueuedRow],
    state: &RebuildState,
) -> Result<()> {
    if stale_rows.is_empty() && rewritten_rows.is_empty() {
        return Ok(());
    }
    let table = table_store::Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS).await?;
    let keys: Vec<_> = stale_rows
        .keys()
        .map(|&(column, id)| (column, id_to_key(id)))
        .collect();
    let deletes: Vec<_> = keys
        .iter()
        .map(|(column, key)| (*column, key.as_slice()))
        .collect();
    let rewritten = rewritten_rows
        .iter()
        .map(|row| {
            let op = if row.column == COL_PENDING {
                state.pending.get(&row.id)
            } else {
                state.dead_letter.get(&row.id)
            }
            .ok_or_else(|| {
                anyhow!(
                    "Rebuilt queue row disappeared: col={} id={}",
                    row.column,
                    row.id
                )
            })?;
            Ok((row.column, id_to_key(row.id), op.to_bytes()?))
        })
        .collect::<Result<Vec<_>>>()?;
    let stores: Vec<_> = rewritten
        .iter()
        .map(|(column, key, bytes)| (*column, key.as_slice(), bytes.as_slice()))
        .collect();
    if let Err(delete_error) = table.apply_batch(&deletes, &stores).await {
        let markers: Vec<_> = stale_rows
            .iter()
            .map(|(&(column, id), stale)| {
                let marker = if is_completion_marker(&stale.op) {
                    stale.op.clone()
                } else {
                    row_cleanup_marker(&stale.op, column)
                };
                Ok((column, id_to_key(id), marker.to_bytes()?))
            })
            .collect::<Result<_>>()?;
        let fallback_stores: Vec<_> = stores
            .iter()
            .copied()
            .chain(
                markers
                    .iter()
                    .map(|(column, key, bytes)| (*column, key.as_slice(), bytes.as_slice())),
            )
            .collect();
        table.apply_batch(&[], &fallback_stores).await.map_err(|marker_error| {
            anyhow!(
                "Failed to durably clean stale queue rows at init: delete={delete_error:#}; marker={marker_error:#}"
            )
        })?;
        for (&(column, id), stale) in stale_rows {
            report_nonfatal(
                "sync.pending_ops",
                "Stored durable cleanup markers after rebuild cleanup failed",
                format!("col={column} id={id}: {}; {delete_error:#}", stale.context),
            );
        }
    }
    Ok(())
}

/// Rebuild the in-memory indexes by scanning the table store once.
pub async fn init_indexes() -> Result<()> {
    let table = table_store::Table::open(PENDING_OPS_TABLE, PENDING_OPS_COLS).await?;
    let mut state = RebuildState::default();

    scan_column(
        &table,
        COL_PENDING,
        &mut state,
        "Removing stale marker from pending column during rebuild",
    )
    .await?;
    scan_column(
        &table,
        COL_DEAD_LETTER,
        &mut state,
        "Removing stale marker from dead-letter column during rebuild",
    )
    .await?;
    prune_shadowed_rebuild_rows(&mut state);
    prune_dedup_rebuild_rows(&mut state);

    let rebuilt = build_rebuilt_indexes(&state);
    cleanup_rebuild_rows(&state.stale_rows, &state.rewritten_rows, &state).await?;
    drop(table);

    *indexes().write().await = rebuilt;
    seed_next_id(state.max_id.saturating_add(1));
    SEQ_COUNTER.store(
        state.max_seq.saturating_add(SEQ_RESTART_GAP),
        Ordering::Release,
    );
    refresh_sync_status().await;
    Ok(())
}
