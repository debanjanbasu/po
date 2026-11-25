//! Generic per-key async mutex registry.
//!
//! [`KeyedMutex`] hands out one async mutex per unique key, creating entries
//! on demand and pruning idle ones (where the `Arc` has no other holders)
//! periodically during lock calls to prevent unbounded growth without O(n)
//! scans each time. Both waiters and holders keep the `Arc` alive, so an
//! in-use lock is never pruned.
//!
//! The module-level [`lock_store`] registry serializes local record
//! mutations: it prevents read/modify/write races on `MessagePage`,
//! `ConversationIndex`, `ContactList`, and other locally stored models. Each
//! unique key (typically username or conversation key bytes) gets its own
//! lock. Veilid's per-record open/close scopes use their own
//! `KeyedMutex<RecordKey>` in `crate::veilid::store`.

use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc, LazyLock, LockResult, Mutex as StdMutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::sync::{Mutex, OwnedMutexGuard};

/// Registry of per-key async mutexes.
///
/// Acquire with [`KeyedMutex::lock`]; the returned guard owns the per-key
/// mutex for its lifetime. The internal map lock is held only long enough
/// to clone the per-key `Arc`, so slow work under one key never stalls
/// lock acquisition on another.
pub struct KeyedMutex<K> {
    inner: StdMutex<HashMap<K, Arc<Mutex<()>>>>,
    accesses: AtomicUsize,
}

impl<K: Eq + Hash> KeyedMutex<K> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: StdMutex::new(HashMap::new()),
            accesses: AtomicUsize::new(0),
        }
    }

    /// Acquires the lock for `key`, waiting if another task holds it.
    ///
    /// Hold the guard across load → modify → save sequences to prevent
    /// concurrent mutations from clobbering each other.
    pub async fn lock(&self, key: K) -> OwnedMutexGuard<()> {
        let mutex = {
            let mut map = recover_poison(self.inner.lock());
            self.maybe_prune(&mut map);
            Arc::clone(map.entry(key).or_default())
        };
        mutex.lock_owned().await
    }

    /// Acquires the lock by borrowed lookup key `Q`, allocating `K` only on miss.
    pub async fn lock_borrowed<Q>(&self, key: &Q) -> OwnedMutexGuard<()>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ToOwned<Owned = K> + ?Sized,
    {
        let mutex = {
            let mut map = recover_poison(self.inner.lock());
            self.maybe_prune(&mut map);
            map.get(key).cloned().unwrap_or_else(|| {
                let m = Arc::new(Mutex::new(()));
                map.insert(key.to_owned(), Arc::clone(&m));
                m
            })
        };
        mutex.lock_owned().await
    }

    fn maybe_prune(&self, map: &mut HashMap<K, Arc<Mutex<()>>>) {
        // Prune periodically to avoid O(n) scans on every acquisition.
        // Entries with strong_count > 1 have a holder or waiter and are
        // kept; pruning an idle entry is safe because the next lock call
        // simply re-creates it.
        if self
            .accesses
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(64)
        {
            map.retain(|_, v| Arc::strong_count(v) > 1);
        }
    }
}

impl<K: Eq + Hash> Default for KeyedMutex<K> {
    fn default() -> Self {
        Self::new()
    }
}

/// Global registry for local store keys.
static STORE_LOCKS: LazyLock<KeyedMutex<Vec<u8>>> = LazyLock::new(KeyedMutex::new);

/// Acquires the per-key lock for the given store key.
///
/// Hold this guard across load → modify → save sequences to prevent
/// concurrent mutations from clobbering each other.
pub async fn lock_store(key: &[u8]) -> OwnedMutexGuard<()> {
    STORE_LOCKS.lock_borrowed(key).await
}

/// Recovers a poisoned `std::sync` lock by taking the inner guard.
///
/// Poisoning only means another task panicked while holding the lock; the
/// guarded data itself stays consistent under our usage, so recovering is
/// the repo-wide convention for `std::sync` locks.
#[must_use]
pub fn recover_poison<T>(result: LockResult<T>) -> T {
    result.unwrap_or_else(PoisonError::into_inner)
}
