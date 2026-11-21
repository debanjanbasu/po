//! Background dictionary training task.
//!
//! Trains a unified compression dictionary from learned samples reactively —
//! wakes via `tokio::sync::Notify` when new samples are flushed, and re-arms
//! itself only for the remaining debounce window. CPU-heavy work runs in
//! `spawn_blocking`.

use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Error, Result};
use tokio::{select, sync::Notify};
use zstd::{bulk::Compressor, dict::from_samples, zstd_safe::compress_bound};

use crate::{
    config,
    helpers::{
        async_utils::{now_epoch_millis, now_epoch_secs, sleep_ms},
        capnp::capnp_u32,
        spawn_blocking,
    },
    models::pending_operation::OpType,
    nonfatal,
    session::epoch::{SessionGuard, current_epoch, spawn_session_scoped},
    storage::{
        background_errors::report_nonfatal,
        training_status::{set_training_idle, set_training_in_progress},
    },
    user::ctx::UserCtx,
};

use super::{codec, samples, training_queue};
/// Base training samples embedded at compile time (unified structured + chat).
const BASE_SAMPLES: &str = include_str!("../../assets/dict/samples/structured.txt");
static BASE_SAMPLE_LINES: LazyLock<Vec<&'static [u8]>> = LazyLock::new(|| {
    BASE_SAMPLES
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty() && !trimmed.starts_with('#')).then_some(trimmed.as_bytes())
        })
        .collect()
});

/// Notification signal — set by [`sample_collector`] when new samples are persisted.
pub static TRAIN_NOTIFY: Notify = Notify::const_new();
static TRAINER_EPOCH_STARTED: AtomicU64 = AtomicU64::new(u64::MAX);

/// Shortest re-arm when a wake cannot train yet. Bounds the sub-second slack of
/// the epoch-second debounce comparison, and gives a failed attempt (whose
/// timestamp read may itself be what failed) a retry frequent enough to
/// recover but rare enough not to log-spam.
const TRAIN_RETRY_FLOOR_MS: u64 = 60_000;

struct TrainingStatusGuard {
    completed: bool,
}

impl TrainingStatusGuard {
    fn start() -> Self {
        set_training_in_progress();
        Self { completed: false }
    }
}

impl Drop for TrainingStatusGuard {
    fn drop(&mut self) {
        set_training_idle(self.completed.then(now_epoch_millis));
    }
}

/// Spawn the background training task.
pub fn spawn_trainer() {
    let epoch = current_epoch();
    if TRAINER_EPOCH_STARTED.swap(epoch, Ordering::SeqCst) == epoch {
        return;
    }
    spawn_session_scoped(async move {
        // `None` = nothing pending, so there is no timer to arm: the loop parks
        // on `TRAIN_NOTIFY` (a flush stores a permit even while this task is
        // training, so no wake is lost). Any wake re-runs `maybe_train`, which
        // re-reads the account and epoch before touching anything.
        let mut retry_ms: Option<u64> = None;
        loop {
            match retry_ms {
                Some(ms) => {
                    select! {
                        () = TRAIN_NOTIFY.notified() => {}
                        () = sleep_ms(ms) => {}
                    }
                }
                None => TRAIN_NOTIFY.notified().await,
            }
            retry_ms = match maybe_train().await {
                Ok(next) => next,
                Err(error) => {
                    // Bounded retry — a failed read/write must not park the loop
                    // on `TRAIN_NOTIFY` forever.
                    report_nonfatal("compression.trainer", "Dictionary training failed", error);
                    Some(TRAIN_RETRY_FLOOR_MS)
                }
            };
        }
    });
}

/// Check conditions and train if appropriate.
///
/// Returns the delay until the next scheduled attempt when training cannot run
/// yet, or `None` when nothing is pending and the caller should block on
/// [`TRAIN_NOTIFY`] with no timer.
async fn maybe_train() -> Result<Option<u64>> {
    let Some((ctx, guard)) = UserCtx::try_current_with_guard() else {
        return Ok(None);
    };
    // Debounce — don't train more than once per hour for this account.
    let last_training = training_queue::get_last_training(&ctx.account_id).await?;
    if !guard.is_current() {
        return Ok(None);
    }
    let now = now_epoch_secs();
    let elapsed = now.saturating_sub(last_training);
    if elapsed < config::compression::MIN_TRAINING_INTERVAL_SECS {
        // Sleep exactly to the interval expiry, never below the floor (the
        // epoch-second comparison leaves sub-second slack that would spin).
        return Ok(Some(
            config::compression::MIN_TRAINING_INTERVAL_SECS
                .saturating_sub(elapsed)
                .saturating_mul(1_000)
                .max(TRAIN_RETRY_FLOOR_MS),
        ));
    }

    // Drain any remaining in-flight samples for the same account and epoch.
    samples::flush_for_account(&ctx, guard).await?;
    if !guard.is_current() {
        return Ok(None);
    }

    // Load patterns ONCE
    let (chat_patterns, structured_patterns) =
        match training_queue::load_all_patterns(&ctx.account_id).await {
            Ok(patterns) => patterns,
            // Stamp failed attempts too so persistent load errors stay debounced
            // to the training interval instead of retraining on every wake.
            Err(e) => {
                training_queue::touch_last_training(&ctx, guard).await?;
                return Err(e);
            }
        };
    if !guard.is_current() {
        return Ok(None);
    }

    let total = chat_patterns.word_freq.len()
        + chat_patterns.emoji_freq.len()
        + structured_patterns.word_freq.len();

    if total < config::compression::MIN_TRAINING_SAMPLES {
        return Ok(None);
    }

    // Past the threshold, training is the expensive half: zstd dictionary
    // training over the whole aggregated corpus plus two comparison
    // compressions, inline on the single thread web has. Defer while the page is
    // visible and come back on the resume timer. Deliberately *not* stamped
    // through `touch_last_training`: a deferral is not an attempt, so the next
    // pass re-tests the same interval rather than being debounced an hour for
    // having looked.
    if crate::platform::lifecycle::defer_heavy_work() {
        return Ok(Some(config::compression::DEFERRED_RESUME_MS));
    }

    let trained_chat_revision = chat_patterns.last_updated;
    let trained_structured_revision = structured_patterns.last_updated;
    let mut training_status = TrainingStatusGuard::start();
    let swapped = match train_and_swap(chat_patterns, structured_patterns).await {
        Ok(swapped) => swapped,
        // Same as the load path: a failed attempt must re-arm the debounce.
        Err(e) => {
            training_queue::touch_last_training(&ctx, guard).await?;
            return Err(e);
        }
    };
    if !guard.is_current() {
        return Ok(None);
    }
    training_status.completed = true;

    // No swap means the samples were never consumed — keep them (and skip
    // the publish) but still stamp the attempt so the interval debounce
    // re-arms; otherwise a persistently below-threshold dictionary retrains
    // on every wake.
    if !swapped {
        training_queue::touch_last_training(&ctx, guard).await?;
        return Ok(None);
    }

    enqueue_dict_publish(ctx.clone(), guard);

    training_queue::reset_after_training(
        &ctx,
        guard,
        trained_chat_revision,
        trained_structured_revision,
    )
    .await?;
    Ok(None)
}

/// Train a unified dictionary and hot-swap if improvement is significant.
/// Returns `true` when a new dictionary was actually swapped in.
///
/// Both chat and structured patterns are merged into a single training corpus.
/// CPU-bound work runs in `spawn_blocking`.
async fn train_and_swap(
    chat_patterns: training_queue::AggregatedPatterns,
    structured_patterns: training_queue::AggregatedPatterns,
) -> Result<bool> {
    let Some((dict, improvement)) = spawn_blocking(move || {
        let mut learned_samples = training_queue::patterns_to_training_samples(&chat_patterns);
        learned_samples.extend(training_queue::patterns_to_training_samples(
            &structured_patterns,
        ));

        if BASE_SAMPLE_LINES.is_empty() && learned_samples.is_empty() {
            return Ok::<_, Error>(None);
        }

        let refs: Vec<&[u8]> = BASE_SAMPLE_LINES
            .iter()
            .copied()
            .chain(learned_samples)
            .collect();
        let dict = from_samples(&refs, config::compression::DICT_SIZE)
            .context("dictionary training failed")?;
        let test_data = build_test_data(&chat_patterns);
        let improvement = compare_improvement(&dict, &test_data)?;

        Ok(Some((dict, improvement)))
    })
    .await??
    else {
        return Ok(false);
    };

    if improvement < config::compression::MIN_IMPROVEMENT_PERCENT {
        return Ok(false);
    }

    codec::swap_dict(dict).await?;

    Ok(true)
}

/// Exact f64 conversion for compression-ratio math: dictionary/test sizes are
/// far below 2^32, so converting via u32 keeps f64 exact.
fn size_f64(size: usize) -> f64 {
    f64::from(capnp_u32(size))
}

/// Fixed chat test lines used when no learned chat patterns exist.
const FIXED_CHAT_TEST_LINES: &[&[u8]] = &[
    b"alice: Hey! How's it going?",
    b"bob: Pretty good! Just finished lunch",
    b"alice: Nice! What did you have?",
    b"bob: Homemade pizza, turned out great!",
    b"system: User joined the conversation",
    b"alice: You should come over sometime!",
    b"bob: I'd love that! When?",
    b"alice: How about this weekend?",
    b"bob: Works for me! Saturday afternoon?",
    b"alice: Perfect! I'll send you the address",
];

/// Fixed structured probes — always included so structured coverage never
/// disappears from the improvement measurement.
const FIXED_STRUCTURED_TEST_LINES: &[&[u8]] = &[
    b"username\x00alice\x00dob\x0019900101\x00terms_accepted\x00true",
    b"username\x00bob\x00dob\x0019850515\x00terms_accepted\x00true",
    b"issuer\x00Google\x00account_name\x00alice@example.com\x00secret\x00JBSWY3DPEHPK3PXP",
    b"contacts\x00id\x00alice:dhtkey123\x00username\x00alice\x00status\x00active",
    b"nav_order\x00TOTP\x00AI\x00Chat\x00Calendar\x00Calls\x00Account",
];

/// Build the improvement-measurement test set: top-K (K=10) learned chat
/// vocabulary plus fixed structured probes. Deterministic: sorted by count
/// desc then key asc. Falls back to fixed chat lines when no chat patterns
/// were learned.
fn build_test_data(chat_patterns: &training_queue::AggregatedPatterns) -> Vec<Vec<u8>> {
    let mut entries: Vec<_> = chat_patterns.word_freq.iter().collect();
    entries.sort_unstable_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let mut chat_lines: Vec<Vec<u8>> = entries
        .into_iter()
        .take(10)
        .map(|(key, _)| key.as_bytes().to_vec())
        .collect();
    if chat_lines.is_empty() {
        chat_lines = FIXED_CHAT_TEST_LINES
            .iter()
            .map(|line| line.to_vec())
            .collect();
    }
    chat_lines.extend(FIXED_STRUCTURED_TEST_LINES.iter().map(|line| line.to_vec()));
    chat_lines
}

/// Compare improvement of new dictionary against current one.
fn compare_improvement(new_dict: &[u8], test_data: &[Vec<u8>]) -> Result<f64> {
    let test_data: Vec<&[u8]> = test_data.iter().map(Vec::as_slice).collect();

    let version = codec::current_version();

    let Some(current) = codec::get_prepared_dict(version) else {
        // No existing dictionary — treat new dict as significant improvement
        return Ok(config::compression::MIN_IMPROVEMENT_PERCENT + 1.0);
    };

    let (total, count) = measure_improvement(&current, new_dict, &test_data)?;
    // count is always > 0: test_data has 15 non-empty items and zstd never
    // produces empty output for non-empty input.
    Ok(total / size_f64(count))
}

/// Measure compression improvement for a set of test data.
fn measure_improvement(
    baseline_dict: &codec::PreparedDict,
    new_dict: &[u8],
    test_data: &[&[u8]],
) -> Result<(f64, usize)> {
    let mut baseline_compressor =
        Compressor::with_dictionary(codec::ZSTD_LEVEL, baseline_dict.bytes())?;
    let mut new_compressor = Compressor::with_dictionary(codec::ZSTD_LEVEL, new_dict)?;
    let max_input = test_data.iter().map(|d| d.len()).max().unwrap_or(0);
    let mut scratch = Vec::with_capacity(compress_bound(max_input));
    let mut total = 0.0f64;
    let mut count = 0usize;

    for data in test_data {
        let baseline_size = size_f64(baseline_compressor.compress_to_buffer(data, &mut scratch)?);
        let new_size = size_f64(new_compressor.compress_to_buffer(data, &mut scratch)?);
        if baseline_size > 0.0 {
            total = ((baseline_size - new_size) / baseline_size).mul_add(100.0, total);
            count += 1;
        }
    }

    Ok((total, count))
}

// ── DHT Publishing via pending_ops ────────────────────────────────────────────

/// Enqueue a `DictPublish` operation after successful training.
///
/// The identity gate lives in `OpType::requires_identity`, consulted by
/// `UserCtx::enqueue`.
fn enqueue_dict_publish(ctx: UserCtx, guard: SessionGuard) {
    if !guard.is_current() {
        return;
    }
    spawn_session_scoped(async move {
        let result: Result<()> = try {
            let user_lock = ctx.lock_for_account_commit(guard).await?;
            if user_lock.is_some() {
                ctx.enqueue(OpType::DictPublish, Vec::<u8>::new()).await?;
            }
        };
        nonfatal!(
            "compression.trainer",
            "Failed to enqueue DictPublish",
            result
        );
    });
}
