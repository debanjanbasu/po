//! Sample collection for dictionary training.
//!
//! Collects chat and structured patterns from user data using an LRU cache
//! to limit memory usage. Patterns are periodically flushed to the training
//! queue for persistence. When patterns are persisted, the trainer is notified
//! via [`dict_trainer::TRAIN_NOTIFY`] to wake reactively.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use anyhow::Result;
use lru::LruCache;

use crate::{
    compression::training_queue::{
        ChatPattern, StructuredPattern, cap_freq_map, extract_structured_pattern,
    },
    config,
    helpers::{spawn, store_lock::recover_poison},
    nonfatal,
    session::epoch::SessionGuard,
    storage::background_errors::report_nonfatal,
    user::ctx::UserCtx,
};

/// Maximum entries in each LRU cache.
const LRU_CAPACITY: NonZeroUsize = match NonZeroUsize::new(config::compression::SAMPLE_LRU_CAPACITY)
{
    Some(v) => v,
    None => unreachable!(),
};

/// Global sample collector instance.
static COLLECTOR: Mutex<Option<SampleCollector>> = Mutex::new(None);

/// Pending sample count for automatic flush trigger.
static PENDING_COUNT: AtomicUsize = AtomicUsize::new(0);
static COLLECTOR_GENERATION: AtomicUsize = AtomicUsize::new(0);
static FLUSH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Sample collector with an account-scoped LRU cache of training patterns.
pub struct SampleCollector {
    account_id: Option<Arc<str>>,
    chat: LruCache<PatternHash, ChatPattern>,
    structured: LruCache<PatternHash, StructuredPattern>,
}

/// Hash wrapper for pattern deduplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PatternHash(u64);

impl PatternHash {
    fn from_chat(pattern: &ChatPattern) -> Self {
        let mut hasher = DefaultHasher::new();

        let mut words: Vec<_> = pattern.word_freq.iter().collect();
        words.sort_unstable_by(|a, b| a.0.cmp(b.0));
        words.len().hash(&mut hasher);
        for word in words {
            word.hash(&mut hasher);
        }

        let mut emojis: Vec<_> = pattern.emoji_freq.iter().collect();
        emojis.sort_unstable_by(|a, b| a.0.cmp(b.0));
        emojis.len().hash(&mut hasher);
        for emoji in emojis {
            emoji.hash(&mut hasher);
        }

        let mut templates: Vec<_> = pattern.templates.iter().collect();
        templates.sort_unstable();
        templates.len().hash(&mut hasher);
        for template in templates {
            template.hash(&mut hasher);
        }

        Self(hasher.finish())
    }

    fn from_structured(pattern: &StructuredPattern) -> Self {
        let mut hasher = DefaultHasher::new();
        pattern.template.hash(&mut hasher);
        pattern.keys.len().hash(&mut hasher);
        for key in &pattern.keys {
            key.hash(&mut hasher);
        }
        Self(hasher.finish())
    }
}

impl SampleCollector {
    fn new() -> Self {
        Self {
            account_id: None,
            chat: LruCache::new(LRU_CAPACITY),
            structured: LruCache::new(LRU_CAPACITY),
        }
    }

    fn insert_chat(&mut self, hash: PatternHash, pattern: ChatPattern) {
        self.chat.put(hash, pattern);
    }

    fn insert_structured(&mut self, hash: PatternHash, pattern: StructuredPattern) {
        self.structured.put(hash, pattern);
    }

    fn clear(&mut self) {
        self.chat.clear();
        self.structured.clear();
        self.account_id = None;
    }

    fn drain(&mut self) -> (Vec<ChatPattern>, Vec<StructuredPattern>) {
        // pop_lru yields oldest-first, matching the previous drain_values order.
        (pop_all(&mut self.chat), pop_all(&mut self.structured))
    }

    fn restore(&mut self, chat: Vec<ChatPattern>, structured: Vec<StructuredPattern>) {
        let (new_chat, new_structured) = self.drain();
        for pattern in chat.into_iter().chain(new_chat) {
            self.insert_chat(PatternHash::from_chat(&pattern), pattern);
        }
        for pattern in structured.into_iter().chain(new_structured) {
            self.insert_structured(PatternHash::from_structured(&pattern), pattern);
        }
    }
}

/// Removes and returns all values, oldest first.
fn pop_all<V>(cache: &mut LruCache<PatternHash, V>) -> Vec<V> {
    std::iter::from_fn(|| cache.pop_lru())
        .map(|(_, v)| v)
        .collect()
}

/// Acquire the global collector lock, recovering from poison — a poisoned
/// collector still holds consistent training data; erroring here would drop
/// samples for the rest of the session.
fn lock_collector() -> MutexGuard<'static, Option<SampleCollector>> {
    recover_poison(COLLECTOR.lock())
}

/// Initialize the global sample collector.
pub fn init() {
    let mut collector = lock_collector();
    COLLECTOR_GENERATION.fetch_add(1, Ordering::Relaxed);
    PENDING_COUNT.store(0, Ordering::Relaxed);
    collector.replace(SampleCollector::new());
}

/// Apply a function to one account's collector and auto-flush at threshold.
fn with_collector(account_id: &str, f: impl FnOnce(&mut SampleCollector)) {
    let mut guard = lock_collector();
    if let Some(collector) = guard.as_mut() {
        if collector.account_id.as_deref() != Some(account_id) {
            collector.clear();
            collector.account_id = Some(Arc::from(account_id));
            COLLECTOR_GENERATION.fetch_add(1, Ordering::Relaxed);
            PENDING_COUNT.store(0, Ordering::Relaxed);
        }
        f(collector);
        if PENDING_COUNT.load(Ordering::Relaxed) >= config::compression::SAMPLE_FLUSH_THRESHOLD
            && !FLUSH_IN_FLIGHT.swap(true, Ordering::AcqRel)
        {
            drop(guard);
            spawn(async {
                nonfatal!("compression.samples", "Auto-flush failed", flush().await);
            });
        }
    }
}

/// Payload ceilings for one collected pattern, applied **at extraction**.
///
/// The collector is the only in-memory holder of these patterns, and the
/// per-entry maps it stores are built by `extract_chat_pattern` with no cap of
/// their own — the 5,000/1,000/500-entry caps exist, but they run at
/// *persist* time (`training_queue::persist_samples`), after every collected
/// pattern has already sat in the LRU. A single 16 KB message of distinct
/// tokens yields ~16,000 n-gram entries (~1 MB of `String`s), so 10,000
/// un-capped entries would peak near 1 GB on a device whose resident budget is
/// measured in tens of MB — for tables that exist only to train a
/// [`config::compression::DICT_SIZE`] (16 KB) dictionary.
///
/// These are the *collector* ceilings; the persist-time caps stay in place and
/// keep bounding the stored aggregate independently.
const COLLECTOR_MAX_WORD_ENTRIES: usize = 512;
const COLLECTOR_MAX_EMOJI_ENTRIES: usize = 64;
const COLLECTOR_MAX_TEMPLATES: usize = 8;

/// Drops the least-frequent entries past `max`, keeping the frequency profile
/// that dictionary training weights by. Mirrors the aggregate cap so a pattern
/// that survives collection is already within the persist-time bounds.
fn cap_pattern(pattern: &mut ChatPattern) {
    cap_freq_map(&mut pattern.word_freq, COLLECTOR_MAX_WORD_ENTRIES);
    cap_freq_map(&mut pattern.emoji_freq, COLLECTOR_MAX_EMOJI_ENTRIES);
    pattern.templates.truncate(COLLECTOR_MAX_TEMPLATES);
}

/// Collect a chat pattern for one account. Triggers auto-flush at threshold.
pub fn collect_chat(account_id: &str, mut pattern: ChatPattern) {
    cap_pattern(&mut pattern);
    let hash = PatternHash::from_chat(&pattern);
    with_collector(account_id, |collector| {
        collector.insert_chat(hash, pattern);
        PENDING_COUNT.fetch_add(1, Ordering::Relaxed);
    });
}

/// Convenience: extract a structured pattern from field names and collect it for one account.
pub fn collect_fields(account_id: &str, keys: &[&str]) {
    let pattern = extract_structured_pattern(keys);
    let hash = PatternHash::from_structured(&pattern);
    with_collector(account_id, |collector| {
        collector.insert_structured(hash, pattern);
        PENDING_COUNT.fetch_add(1, Ordering::Relaxed);
    });
}

struct FlushInFlightGuard;

impl Drop for FlushInFlightGuard {
    fn drop(&mut self) {
        FLUSH_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Flush samples for the session active when the flush begins.
pub async fn flush() -> Result<()> {
    let _in_flight = FlushInFlightGuard;
    let Some((ctx, guard)) = UserCtx::try_current_with_guard() else {
        return Ok(());
    };
    flush_for_account(&ctx, guard).await
}

/// Flush collected samples under the captured account's durable commit fence.
pub async fn flush_for_account(ctx: &UserCtx, guard: SessionGuard) -> Result<()> {
    let _in_flight = FlushInFlightGuard;
    if !guard.is_current() {
        return Ok(());
    }
    let (generation, account_id, drained_count, chat, structured) = {
        let mut collector_guard = lock_collector();
        if !guard.is_current() {
            return Ok(());
        }
        let Some(collector) = collector_guard.as_mut() else {
            PENDING_COUNT.store(0, Ordering::Relaxed);
            return Ok(());
        };
        if collector.account_id.as_deref() != Some(ctx.account_id.as_ref()) {
            return Ok(());
        }
        let generation = COLLECTOR_GENERATION.load(Ordering::Relaxed);
        let account_id = Arc::clone(&ctx.account_id);
        let drained_count = PENDING_COUNT.swap(0, Ordering::Relaxed);
        let (chat, structured) = collector.drain();
        let result = (generation, account_id, drained_count, chat, structured);
        drop(collector_guard);
        result
    };

    if chat.is_empty() && structured.is_empty() {
        return Ok(());
    }

    match super::training_queue::persist_samples(ctx, guard, &chat, &structured).await {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(error) => {
            let mut collector_guard = lock_collector();
            if guard.is_current()
                && COLLECTOR_GENERATION.load(Ordering::Relaxed) == generation
                && let Some(collector) = collector_guard.as_mut()
                && collector.account_id.as_deref() == Some(&account_id)
            {
                collector.restore(chat, structured);
                PENDING_COUNT.fetch_add(drained_count, Ordering::Relaxed);
            } else if guard.is_current() {
                report_nonfatal(
                    "compression.samples",
                    "Dropped training samples for previous account after failed flush",
                    format!("{error:#}; {drained_count} sample(s) discarded on account switch"),
                );
            }
            drop(collector_guard);
            return Err(error);
        }
    }

    super::trainer::TRAIN_NOTIFY.notify_one();
    Ok(())
}

/// Clears the global sample collector (e.g. on logout).
pub fn clear_collector() {
    let mut guard = lock_collector();
    COLLECTOR_GENERATION.fetch_add(1, Ordering::Relaxed);
    PENDING_COUNT.store(0, Ordering::Relaxed);
    if let Some(collector) = guard.as_mut() {
        collector.clear();
    }
}
