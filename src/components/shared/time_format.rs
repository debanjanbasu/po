//! UI-facing time formatting helpers (relative timestamps, uptime, local date-time).

use std::{borrow::Cow, cell::RefCell, num::NonZeroUsize, sync::Arc};

use chrono::{DateTime, Local, Utc};
use lru::LruCache;

use crate::helpers::async_utils::now_epoch_millis;

/// Formats a millisecond timestamp as a relative time string (e.g. "5m ago", "2h ago").
///
/// Returns an empty string for timestamp 0, or "Just now" for sub-minute elapsed.
pub fn format_relative_time(millis: u64) -> Cow<'static, str> {
    format_relative_time_at(now_epoch_millis(), millis)
}

/// Formats a millisecond timestamp as a relative time string using a caller-provided `now`.
///
/// Useful when formatting many timestamps in one render pass, avoiding repeated clock reads.
pub fn format_relative_time_at(now_millis: u64, millis: u64) -> Cow<'static, str> {
    if millis == 0 {
        return Cow::Borrowed("");
    }
    let elapsed = now_millis.saturating_sub(millis);
    let mins = elapsed / 60_000;
    let hours = mins / 60;
    let days = hours / 24;

    if days > 0 {
        Cow::Owned(format!("{days}d ago"))
    } else if hours > 0 {
        Cow::Owned(format!("{hours}h ago"))
    } else if mins > 0 {
        Cow::Owned(format!("{mins}m ago"))
    } else {
        Cow::Borrowed("Just now")
    }
}

/// Formats a duration in seconds as a compact uptime string (e.g. "42s", "7m", "3h").
pub fn format_uptime(secs: u64) -> String {
    const SECS_PER_MINUTE: u64 = 60;
    const SECS_PER_HOUR: u64 = 3_600;
    if secs >= SECS_PER_HOUR {
        format!("{}h", secs / SECS_PER_HOUR)
    } else if secs >= SECS_PER_MINUTE {
        format!("{}m", secs / SECS_PER_MINUTE)
    } else {
        format!("{secs}s")
    }
}

/// Formats epoch millis as local `YYYY-MM-DD HH:MM`.
///
/// The result is cached because a conversation view formats one timestamp per
/// rendered bubble: a 2,000-message history meant 2,000 timezone conversions
/// and 2,000 string allocations on first paint, repeated for every bubble
/// that re-rendered. Message timestamps are immutable, so a timestamp maps to
/// exactly one string.
///
/// The cache is a bounded [`lru::LruCache`] (true least-recently-used eviction,
/// constant-time hits — no linear scan or vector compaction), so it cannot
/// grow without limit as history ages. It is sized for the message window
/// the conversation view renders plus a margin — a few hundred entries at
/// most, each a short string. Hits return the cached [`Arc<str>`] directly,
/// so a repeat render of a bubble clones an atomic reference count instead of
/// the string contents.
#[must_use]
pub fn format_local_ymdhm_from_millis(ms: u64) -> Arc<str> {
    if let Some(cached) = timestamp_cache_get(ms) {
        return cached;
    }
    let formatted: Arc<str> = i64::try_from(ms)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|utc| utc.with_timezone(&Local))
        .map_or_default(|local| Arc::from(local.format("%Y-%m-%d %H:%M").to_string()));
    timestamp_cache_put(ms, Arc::clone(&formatted));
    formatted
}

/// Entries kept in the timestamp cache.
///
/// Sized for the conversation view's message window (`config::chat::WINDOW_MESSAGES`)
/// plus room for a second window's worth of distinct timestamps, so a reader
/// scrolling through history does not thrash the cache.
const TIMESTAMP_CACHE_CAPACITY: usize = 256;

thread_local! {
    /// Cached `timestamp -> formatted string` pairs in true LRU order.
    ///
    /// Not a `const` initializer: `LruCache::new` is a runtime constructor.
    static TIMESTAMP_CACHE: RefCell<LruCache<u64, Arc<str>>> = RefCell::new(LruCache::new(
        NonZeroUsize::new(TIMESTAMP_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN),
    ));
}

fn timestamp_cache_get(ms: u64) -> Option<Arc<str>> {
    TIMESTAMP_CACHE.with(|cache| cache.borrow_mut().get(&ms).cloned())
}

fn timestamp_cache_put(ms: u64, formatted: Arc<str>) {
    TIMESTAMP_CACHE.with(|cache| {
        cache.borrow_mut().put(ms, formatted);
    });
}
