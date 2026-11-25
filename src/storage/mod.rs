//! Persistence and sync infrastructure.

pub mod account_table;
pub mod automerge;
pub mod background_errors;
pub mod backup;
pub mod schema_version;
pub mod snapshots;
pub mod store_trait;

use dioxus::prelude::*;

fn set_if_changed<T: PartialEq + 'static>(signal: &GlobalSignal<T>, next: T) {
    if *signal.peek() != next {
        *signal.write() = next;
    }
}

pub mod sync_status {
    //! Sync status tracking for offline-first UI feedback.

    use dioxus::prelude::*;

    use crate::sync::pending_ops;

    use super::set_if_changed;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum SyncPhase {
        #[default]
        Idle,
        Processing {
            pending: u32,
            dead_letter: u32,
        },
        Drained {
            dead_letter: u32,
        },
    }

    impl SyncPhase {
        pub const fn from_counts(pending: u32, dead_letter: u32) -> Self {
            match (pending, dead_letter) {
                (0, 0) => Self::Idle,
                (0, dead_letter) => Self::Drained { dead_letter },
                (pending, dead_letter) => Self::Processing {
                    pending,
                    dead_letter,
                },
            }
        }

        pub const fn pending_count(&self) -> u32 {
            match self {
                Self::Processing { pending, .. } => *pending,
                Self::Idle | Self::Drained { .. } => 0,
            }
        }

        pub const fn dead_letter_count(&self) -> u32 {
            match self {
                Self::Idle => 0,
                Self::Processing { dead_letter, .. } | Self::Drained { dead_letter } => {
                    *dead_letter
                }
            }
        }

        pub const fn is_processing(&self) -> bool {
            matches!(self, Self::Processing { .. })
        }
    }

    pub static SYNC_STATUS: GlobalSignal<SyncPhase> = Signal::global(SyncPhase::default);

    pub async fn refresh_sync_status() {
        let (pending, dead) = pending_ops::queue_counts().await;
        set_if_changed(&SYNC_STATUS, SyncPhase::from_counts(pending, dead));
    }

    pub fn mark_sync_processing_started() {
        let next = {
            let phase = SYNC_STATUS.peek();
            SyncPhase::Processing {
                pending: phase.pending_count().max(1),
                dead_letter: phase.dead_letter_count(),
            }
        };
        set_if_changed(&SYNC_STATUS, next);
    }

    pub fn mark_sync_processing_finished() {
        let next = {
            let phase = SYNC_STATUS.peek();
            SyncPhase::from_counts(phase.pending_count(), phase.dead_letter_count())
        };
        set_if_changed(&SYNC_STATUS, next);
    }
}

pub mod training_status {
    //! Training status signal for dictionary training background task.

    use dioxus::prelude::*;

    use super::set_if_changed;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TrainingState {
        Idle { last_trained_epoch_ms: Option<u64> },
        InProgress,
    }

    impl TrainingState {
        pub const fn is_training(&self) -> bool {
            matches!(self, Self::InProgress)
        }

        pub const fn last_trained(&self) -> Option<u64> {
            match self {
                Self::Idle {
                    last_trained_epoch_ms,
                } => *last_trained_epoch_ms,
                Self::InProgress => None,
            }
        }
    }

    pub static TRAINING_STATUS: GlobalSignal<TrainingState> =
        Signal::global(|| TrainingState::Idle {
            last_trained_epoch_ms: None,
        });

    pub fn set_training_idle(last_trained_epoch_ms: Option<u64>) {
        set_if_changed(
            &TRAINING_STATUS,
            TrainingState::Idle {
                last_trained_epoch_ms,
            },
        );
    }

    pub fn set_training_in_progress() {
        set_if_changed(&TRAINING_STATUS, TrainingState::InProgress);
    }
}

pub mod upgrade_status {
    //! Progress tracking for local recompression and remote dictionary sync.

    use std::sync::atomic::{AtomicU64, Ordering};

    use dioxus::prelude::*;

    use super::set_if_changed;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum UpgradeState {
        #[default]
        Idle,
        InProgress {
            total: u64,
            completed: u64,
            run_id: u64,
        },
    }

    impl UpgradeState {
        pub const fn is_upgrading(&self) -> bool {
            matches!(self, Self::InProgress { .. })
        }

        pub const fn progress(&self) -> (u64, u64) {
            match self {
                Self::InProgress {
                    total, completed, ..
                } => (*total, *completed),
                Self::Idle => (0, 0),
            }
        }
    }

    static NEXT_UPGRADE_STATUS_RUN: AtomicU64 = AtomicU64::new(1);

    pub static UPGRADE_STATUS: GlobalSignal<UpgradeState> = Signal::global(UpgradeState::default);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UpgradeStatusToken(u64);

    #[must_use = "dropping the run guard clears its progress if it still owns the status"]
    pub struct UpgradeStatusRun {
        token: UpgradeStatusToken,
    }

    impl UpgradeStatusRun {
        #[must_use]
        pub const fn token(&self) -> UpgradeStatusToken {
            self.token
        }
    }

    impl Drop for UpgradeStatusRun {
        fn drop(&mut self) {
            let should_clear = matches!(
                *UPGRADE_STATUS.peek(),
                UpgradeState::InProgress { run_id, .. } if run_id == self.token.0
            );
            if should_clear {
                *UPGRADE_STATUS.write() = UpgradeState::Idle;
            }
        }
    }

    pub fn set_upgrade_status(total: u64) -> UpgradeStatusRun {
        let mut token = UpgradeStatusToken(0);
        UPGRADE_STATUS.with_mut(|status| {
            token = UpgradeStatusToken(NEXT_UPGRADE_STATUS_RUN.fetch_add(1, Ordering::Relaxed));
            *status = UpgradeState::InProgress {
                total,
                completed: 0,
                run_id: token.0,
            };
        });
        UpgradeStatusRun { token }
    }

    pub fn increment_upgrade_progress(token: UpgradeStatusToken, count: u64) {
        if count == 0 {
            return;
        }
        let should_update = matches!(
            *UPGRADE_STATUS.peek(),
            UpgradeState::InProgress { run_id, .. } if run_id == token.0
        );
        if !should_update {
            return;
        }
        UPGRADE_STATUS.with_mut(|status| {
            if let UpgradeState::InProgress {
                completed, run_id, ..
            } = status
                && *run_id == token.0
            {
                *completed = completed.saturating_add(count);
            }
        });
    }

    pub fn clear_upgrade_status() {
        set_if_changed(&UPGRADE_STATUS, UpgradeState::Idle);
    }
}

pub mod hydration_status {
    //! Hydration status signal for post-pairing conversation sync visibility.

    use dioxus::prelude::*;

    use super::set_if_changed;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct HydrationProgress {
        pub total: u32,
        pub completed: u32,
        pub failed: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum HydrationState {
        #[default]
        Idle,
        InProgress(HydrationProgress),
    }

    impl HydrationState {
        pub const fn is_active(&self) -> bool {
            matches!(self, Self::InProgress(_))
        }

        pub const fn progress(&self) -> Option<HydrationProgress> {
            match self {
                Self::InProgress(p) => Some(*p),
                Self::Idle => None,
            }
        }
    }

    pub static HYDRATION_STATUS: GlobalSignal<HydrationState> =
        Signal::global(HydrationState::default);

    pub fn set_hydration_started(total: u32) {
        set_if_changed(
            &HYDRATION_STATUS,
            HydrationState::InProgress(HydrationProgress {
                total,
                completed: 0,
                failed: 0,
            }),
        );
    }

    pub fn increment_hydration_completed() {
        if !HYDRATION_STATUS.peek().is_active() {
            return;
        }
        HYDRATION_STATUS.with_mut(|s| {
            if let HydrationState::InProgress(p) = s {
                p.completed += 1;
            }
        });
    }

    pub fn increment_hydration_failed() {
        if !HYDRATION_STATUS.peek().is_active() {
            return;
        }
        HYDRATION_STATUS.with_mut(|s| {
            if let HydrationState::InProgress(p) = s {
                p.failed += 1;
            }
        });
    }

    pub fn clear_hydration_status() {
        set_if_changed(&HYDRATION_STATUS, HydrationState::Idle);
    }
}

pub mod pairing_sync_status {
    //! Pairing sync status signal — new-device login progress visibility.

    use std::sync::atomic::{AtomicU64, Ordering};

    use dioxus::prelude::*;

    /// Stages of a new-device pairing sync, in order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum PairingSyncStage {
        #[default]
        Idle,
        /// Waiting for the local Veilid node's DHT to become ready.
        Connecting,
        /// Fetching identity subkeys (profile/security/app-state/pointers).
        Identity,
        /// Record reachable but its onboarding data hasn't been published yet;
        /// re-polling until the sharing device's copy lands.
        WaitingForSource,
        /// Building and admitting the local user snapshot.
        Admission,
        /// Persisting synced app settings locally.
        AppState,
        /// Pulling TOTP/contact/conversation-index child records.
        ChildRecords,
        /// Hydrating conversation history (counts come from `HYDRATION_STATUS`).
        Conversations,
        /// Syncing the compression dictionary.
        Dictionary,
        /// Everything synced; shown briefly, then cleared.
        Complete,
        /// A stage failed and the sync stopped: conversation history may be
        /// absent, so this must never be read as success. Reported with the
        /// failure in the Activity panel; cleared with the run.
        Failed,
    }

    impl PairingSyncStage {
        /// Label for the pairing form while the attempt is still in flight.
        #[must_use]
        pub const fn attempt_label(self) -> Option<&'static str> {
            match self {
                Self::Connecting => Some("Connecting to the network…"),
                Self::Identity => Some(
                    "Fetching your account… This can take a couple of minutes on a fresh device.",
                ),
                Self::WaitingForSource => Some(
                    "Waiting for your other device to finish publishing… Keep it open and online.",
                ),
                Self::Admission => Some("Setting up this device…"),
                Self::AppState | Self::ChildRecords | Self::Conversations | Self::Dictionary => {
                    Some("Syncing your data…")
                }
                Self::Failed => Some("Device sync could not finish. See the Activity panel."),
                Self::Idle | Self::Complete => None,
            }
        }

        /// Label for the global activity banner (post-admission stages only —
        /// earlier stages render inline on the pairing form instead).
        #[must_use]
        pub const fn banner_label(self) -> Option<&'static str> {
            match self {
                Self::AppState => Some("Syncing app settings…"),
                Self::ChildRecords => Some("Syncing account data…"),
                Self::Conversations => Some("Syncing conversations…"),
                Self::Dictionary => Some("Syncing dictionary…"),
                Self::Complete => Some("Device sync complete"),
                Self::Failed => Some("Device sync incomplete — see the Activity panel"),
                Self::Idle
                | Self::Connecting
                | Self::Identity
                | Self::WaitingForSource
                | Self::Admission => None,
            }
        }

        /// True while sync work is still running (excludes `Idle`/`Complete`).
        #[must_use]
        pub const fn is_active(self) -> bool {
            !matches!(self, Self::Idle | Self::Complete)
        }

        /// True when the run stopped on a failure: the banner must not render
        /// the green completion pill for it.
        #[must_use]
        pub const fn is_failed(self) -> bool {
            matches!(self, Self::Failed)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct PairingSyncState {
        pub run_id: u64,
        pub stage: PairingSyncStage,
    }

    pub static PAIRING_SYNC_STATUS: GlobalSignal<PairingSyncState> =
        Signal::global(PairingSyncState::default);

    static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);

    /// Starts a new pairing run, superseding any previous one.
    /// Returns the run id to pass to subsequent [`set_stage`]/[`clear`] calls.
    pub fn begin_run() -> u64 {
        let run_id = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
        *PAIRING_SYNC_STATUS.write() = PairingSyncState {
            run_id,
            stage: PairingSyncStage::Connecting,
        };
        run_id
    }

    /// Advances the stage for `run_id`; ignored when a newer run has started.
    pub fn set_stage(run_id: u64, stage: PairingSyncStage) {
        PAIRING_SYNC_STATUS.with_mut(|state| {
            if state.run_id == run_id {
                state.stage = stage;
            }
        });
    }

    /// Resets to `Idle` if `run_id` is still the active run.
    pub fn clear(run_id: u64) {
        PAIRING_SYNC_STATUS.with_mut(|state| {
            if state.run_id == run_id {
                state.stage = PairingSyncStage::Idle;
            }
        });
    }

    /// Resets to `Idle` unconditionally (user reset the pairing form).
    pub fn clear_active() {
        PAIRING_SYNC_STATUS.with_mut(|state| {
            state.stage = PairingSyncStage::Idle;
        });
    }
}
