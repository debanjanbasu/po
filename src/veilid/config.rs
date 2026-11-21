//! Platform-specific Veilid configuration helpers.

#[cfg(target_os = "android")]
use crate::platform::android_plugin::with_activity;
#[cfg(target_os = "android")]
use jni::{jni_sig, jni_str};
#[cfg(not(target_arch = "wasm32"))]
use std::{env, path::PathBuf};
// `Path` is only needed by the prune: the caller and the prune itself are
// both native-only, so wasm never sees it.
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;

#[cfg(not(target_arch = "wasm32"))]
use crate::config::app::APP_ID;

#[cfg(not(target_arch = "wasm32"))]
const VEILID_SUBDIR: &str = ".veilid";

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn get_base_dir() -> String {
    let base = resolve_base_dir();
    // `PO_INSTANCE` (seam builds only) renames the leaf so two instances can
    // run side by side on one machine — each gets its own data dir, locks,
    // device id and DHT identity (docs/device-testing.md, Mac↔Mac tandem).
    let base = match crate::config::e2e::instance() {
        Some(instance) => base.with_file_name(format!("{VEILID_SUBDIR}-{instance}")),
        None => base,
    };
    // A per-run instance name would otherwise leave one directory behind per
    // run, forever, in a container nobody can reach (the phone's app
    // container). Prune the stale siblings here, where the app can reach its
    // own data dir on every platform.
    prune_stale_instances(&base);
    base.to_string_lossy().into_owned()
}

/// Removes sibling `.veilid-*` data dirs left by earlier harness runs.
///
/// Seam builds only, and only the dirs this app created: the leaf must carry
/// the `.veilid-` prefix and live in the same parent as the current data dir.
///
/// Two rules, so the count is bounded no matter how many runs happen:
/// a dir older than the retention window is removed, and once the surviving
/// set exceeds the ceiling the oldest ones are removed too. A dir younger than
/// the window is another live instance's — two instances can run side by side
/// — so the age rule leaves it alone; the ceiling rule only removes the
/// oldest, and only beyond the limit.
///
/// Best-effort: a failure to remove a stale dir must never stop the app from
/// starting.
#[cfg(not(target_arch = "wasm32"))]
fn prune_stale_instances(current: &Path) {
    /// How long an unused instance dir is kept before it is pruned.
    const RETENTION: std::time::Duration = std::time::Duration::from_hours(24);
    /// The most sibling instance dirs that may exist at once.
    const MAX_INSTANCES: usize = 4;

    let Some(parent) = current.parent() else {
        return;
    };
    let prefix = format!("{VEILID_SUBDIR}-");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    // (path, mtime) of every sibling instance dir except the current one.
    let mut siblings: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        if path == current {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        siblings.push((path, modified));
    }

    // Oldest first, so both rules remove the stalest dirs.
    siblings.sort_by_key(|(_, modified)| *modified);

    let mut keep = 0usize;
    for (path, modified) in siblings {
        let aged_out = modified.elapsed().is_ok_and(|age| age >= RETENTION);
        let over_limit = keep >= MAX_INSTANCES;
        if aged_out || over_limit {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            keep += 1;
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn resolve_base_dir() -> PathBuf {
    #[cfg(target_os = "ios")]
    {
        env::var_os("CFFIXED_USER_HOME")
            .map(PathBuf::from)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/private/var/mobile"))
            .join("Library")
            .join("Application Support")
            .join(APP_ID)
            .join(VEILID_SUBDIR)
    }

    #[cfg(target_os = "android")]
    {
        android_runtime_files_dir().join(VEILID_SUBDIR)
    }

    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        #[cfg(target_os = "macos")]
        {
            env::var_os("XDG_DATA_HOME")
                .or_else(|| env::var_os("HOME"))
                .map_or_else(
                    || PathBuf::from(format!("./{VEILID_SUBDIR}")),
                    |path| {
                        PathBuf::from(path)
                            .join("Library")
                            .join("Application Support")
                            .join(APP_ID)
                            .join(VEILID_SUBDIR)
                    },
                )
        }

        #[cfg(target_os = "windows")]
        {
            env::var_os("APPDATA")
                .or_else(|| env::var_os("LOCALAPPDATA"))
                .map_or_else(
                    || PathBuf::from(format!("./{VEILID_SUBDIR}")),
                    |path| PathBuf::from(path).join(APP_ID).join(VEILID_SUBDIR),
                )
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|home| home.join(".local").join("share"))
                })
                .map_or_else(
                    || PathBuf::from(format!("./{VEILID_SUBDIR}")),
                    |path| path.join(APP_ID).join(VEILID_SUBDIR),
                )
        }
    }
}

#[cfg(target_os = "android")]
fn android_runtime_files_dir() -> PathBuf {
    with_activity(|env, activity| {
        let files_dir = env
            .call_method(
                activity,
                jni_str!("getFilesDir"),
                jni_sig!(() -> java.io.File),
                &[],
            )
            .ok()?
            .l()
            .ok()?;
        let path = env
            .call_method(
                &files_dir,
                jni_str!("getAbsolutePath"),
                jni_sig!(() -> java.lang.String),
                &[],
            )
            .ok()?
            .l()
            .ok()?;
        let path = env.as_cast::<jni::objects::JString>(&path).ok()?;
        Some(PathBuf::from(path.try_to_string(env).ok()?))
    })
    .or_else(|| {
        env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|path| android_app_files_dir(&path))
    })
    .or_else(|| {
        env::current_dir()
            .ok()
            .and_then(|path| android_app_files_dir(&path))
    })
    .unwrap_or_else(|| PathBuf::from(".").join(APP_ID).join("files"))
}

#[cfg(target_os = "android")]
fn android_app_files_dir(path: &Path) -> Option<PathBuf> {
    let package = path
        .ancestors()
        .find(|candidate| candidate.file_name().is_some_and(|name| name == APP_ID))?;
    path.ancestors()
        .find(|candidate| {
            candidate.file_name().is_some_and(|name| name == "files")
                && candidate.starts_with(package)
        })
        .map(Path::to_path_buf)
        .or_else(|| Some(package.join("files")))
}
