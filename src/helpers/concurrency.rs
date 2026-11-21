//! Concurrency primitives for fan-out over independent work.
//!
//! The house default is **unbounded**: every item's future is created and
//! polled at once, so a fan-out costs one task's latency instead of
//! `ceil(n / limit)` rounds of it. What used to be a semaphore budget in these
//! paths (`MAX_HYDRATE_CONCURRENCY`, `SWEEP_CONCURRENCY`, `MAX_SIGNAL_FANOUT`,
//! `RECOMPRESSION_CONCURRENCY`) was a throughput throttle, and the throttle was
//! the wall time of every login, sweep and publish.
//!
//! Ordering is preserved on purpose: callers zip results back against their
//! input (pages, keys, route ids), so a completion-ordered stream would make
//! every call site carry an index. `join_all` keeps the input order for free.
//!
//! Bounding is *not* provided here on purpose: it is for the one case a budget
//! is genuinely load-bearing — a fan-out where *each* in-flight item holds a
//! large allocation, so unbounded would multiply peak heap by the item count.
//! When a site like that appears, add a `map_concurrent_with_budget` next to
//! `map_concurrent` (a `Semaphore` acquired inside each task) and say why at
//! the call site.

use futures_util::future::join_all;

/// Runs `task_fn` over every item with **unbounded** concurrency, returning the
/// results in input order.
///
/// Every future is created up front, so the caller's peak heap grows with the
/// item count times whatever one future holds. That is the intended trade: the
/// items here are cheap handles (keys, ids, small records), and the alternative
/// serializes work that has no reason to wait.
pub async fn map_concurrent<T, F, R>(items: impl IntoIterator<Item = T>, task_fn: F) -> Vec<R>
where
    F: AsyncFn(T) -> R,
{
    join_all(items.into_iter().map(|item| task_fn(item))).await
}

// A fire-and-forget `spawn_concurrent` would belong here too, but it needs
// `helpers::spawn` (the Dioxus-runtime spawn) rather than `tokio::spawn` inside
// it, and no caller needs it yet — add it with its first caller instead of
// carrying an unused fan-out whose spawn choice is easy to get wrong.
