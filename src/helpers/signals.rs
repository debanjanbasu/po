// Dioxus-free signal shim — the domain layer's global reactive state.
//
// Replaces `dioxus::prelude::{GlobalSignal, Signal}` for the domain files:
// the same `peek` / `read` / `write` / `with_mut` / `with` / `set` API over
// a process-global `RwLock`. Reactivity is NOT provided — the gpui side
// observes state through `ui::state`'s entities (RevisionBus, observers)
// instead, which is the migration's contract (docs/migration-gpui-ce.md §3).
//
// `GlobalSignal::global(init)` installs into a process-global registry on
// first access; every accessor for the same type parameter `T`… no — keyed
// by the static's address via a unique name passed at creation. To keep the
// call sites unchanged (`Signal::global(default_fn)`), each static gets its
// own storage: the macro-free approach is a `GlobalSignal<T>` owning its
// `RwLock<T>` directly (no registry), created by `Signal::global(fn)`.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// A process-global value with the signal-shaped API. Owns its storage, so
/// `Signal::global(init)` is just construction.
pub struct GlobalSignal<T: Send + Sync + 'static> {
    cell: RwLock<T>,
}

impl<T: Send + Sync + 'static> GlobalSignal<T> {
    /// Creates the signal with an initial value (the old
    /// `Signal::global(default_fn)` shape: pass the default value).
    pub const fn new(value: T) -> Self {
        Self {
            cell: RwLock::new(value),
        }
    }

    /// Immutable snapshot borrow (the old `peek`/`read`).
    pub fn peek(&self) -> RwLockReadGuard<'_, T> {
        self.cell
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Alias of [`peek`] — `read` at call sites maps here.
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.peek()
    }

    /// Mutable borrow (the old `write`).
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.cell
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs `f` with `&mut T` (the old `with_mut`).
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.write())
    }

    /// Runs `f` with `&T` (the old `with`).
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.peek())
    }

    /// Replaces the value (the old `set`).
    pub fn set(&self, value: T) {
        *self.write() = value;
    }
}

impl<T: Default + Send + Sync + 'static> Default for GlobalSignal<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Mirrors the dioxus `Signal::global(default_fn)` construction shape: takes
/// the default-producing closure so statics read
/// `Signal::global(|| default)` with only the import path swapped.
pub struct Signal;

impl Signal {
    /// Creates a `GlobalSignal` from a default-producing closure.
    pub fn global<T: Send + Sync + 'static>(init: impl FnOnce() -> T) -> GlobalSignal<T> {
        GlobalSignal::new(init())
    }
}
