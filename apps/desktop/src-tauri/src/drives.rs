//! External and removable drives coming and going while the app runs (task 2.5).
//!
//! A background thread ([`start_monitor`]) polls [`volumes::signature`] every
//! [`POLL_INTERVAL`] (a few microseconds on Windows: drive letters, types and serial numbers,
//! no disk access). Only when it changes is the full volume list read and compared with the
//! previous one ([`volumes::diff`], by path *and* serial). Then:
//!
//! 1. `volumes://changed` ([`VOLUMES_EVENT`]) carries the new list to the UI;
//! 2. the [`RootPlan`] is recomputed ([`roots::plan`]) from the indexed-roots choice, the
//!    volumes and the [`DriveSettings`], and handed to
//!    [`crate::indexing::Indexer::reconcile`]: roots whose drive went away go offline (hidden
//!    at once, documents kept), roots on a drive that arrived (or another stick on the same
//!    letter) are walked alone in the background.
//!
//! In "whole computer" mode, the external drives indexed during this session are remembered
//! ([`DriveMonitor`]): when unplugged they stay in the plan, offline, so unplugging only hides
//! their entries and plugging them back walks that drive alone. The memory is per session; at
//! the next start the snapshot of the roots it had is still loaded and the missing drive hidden.
//!
//! Commands: [`get_drive_settings`] / [`set_drive_settings`] (persisted next to
//! `indexed-roots.json`, see [`crate::roots::DRIVE_SETTINGS_FILE`]).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::status_listener;
use crate::indexing::StartOutcome;
use crate::roots::{self, DriveSettings, RootPlan, RootsChoice, key_has_prefix, path_key};
use crate::state::AppState;
use crate::volumes::{self, Signature, Volume, VolumeKind};

/// Event carrying the new volume list (`Volume[]`) whenever a drive is plugged in or removed.
pub const VOLUMES_EVENT: &str = "volumes://changed";

/// How often the drive letters are polled.
pub const POLL_INTERVAL: Duration = Duration::from_millis(2500);

/// What the monitor knows during this session (in [`AppState`]).
#[derive(Debug, Default)]
pub struct DriveMonitor {
    inner: Mutex<MonitorState>,
    started: AtomicBool,
}

#[derive(Debug, Default)]
struct MonitorState {
    /// The volumes at the last check, and their cheap signature.
    volumes: Vec<Volume>,
    signature: Option<Signature>,
    /// External drive roots "the whole computer" indexed this session, with their kind.
    remembered: Vec<(PathBuf, VolumeKind)>,
}

impl DriveMonitor {
    fn lock(&self) -> MutexGuard<'_, MonitorState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the volumes seen at start-up (the first poll compares with them).
    pub fn prime(&self, volumes: Vec<Volume>, signature: Signature) {
        let mut m = self.lock();
        m.volumes = volumes;
        m.signature = Some(signature);
    }

    /// The plan of `choice` with `volumes` and `settings`, remembering the external drives it
    /// indexes (whole computer only) and keeping the remembered ones the settings still take
    /// in (offline while absent).
    pub fn plan(
        &self,
        choice: &RootsChoice,
        volumes: &[Volume],
        settings: DriveSettings,
    ) -> Result<RootPlan, String> {
        let mut m = self.lock();
        m.remembered.retain(|(_, kind)| settings.includes(*kind));
        let remembered: Vec<PathBuf> = match choice {
            RootsChoice::WholeComputer => m.remembered.iter().map(|(r, _)| r.clone()).collect(),
            RootsChoice::Custom(_) => Vec::new(),
        };
        let plan = roots::plan(choice, volumes, settings, &remembered)?;
        if *choice == RootsChoice::WholeComputer {
            for v in volumes {
                let root = PathBuf::from(&v.path);
                if v.kind != VolumeKind::Fixed
                    && plan.roots.contains(&root)
                    && !m.remembered.iter().any(|(r, _)| *r == root)
                {
                    m.remembered.push((root, v.kind));
                }
            }
        }
        Ok(plan)
    }

    /// Checks the drives once: `None` if nothing changed since the last check, else the new
    /// volume list and the roots of `plan_roots` lying on a volume that arrived.
    fn check(&self) -> Option<(Vec<Volume>, volumes::VolumeChanges)> {
        let signature = volumes::signature();
        if self.lock().signature.as_ref() == Some(&signature) {
            return None;
        }
        let list = volumes::list_volumes();
        let mut m = self.lock();
        let changes = volumes::diff(&m.volumes, &list);
        m.volumes = list.clone();
        m.signature = Some(signature);
        (!changes.is_empty()).then_some((list, changes))
    }
}

/// The roots of `roots` that lie on one of `volumes` (never counting `/`, which holds
/// everything): the roots to walk again when those volumes arrive.
pub fn roots_on(roots: &[PathBuf], volumes: &[Volume]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter(|r| {
            let key = path_key(r);
            volumes.iter().any(|v| {
                let vkey = path_key(Path::new(&v.path));
                !vkey.is_empty() && key_has_prefix(&key, &vkey)
            })
        })
        .cloned()
        .collect()
}

/// The current plan (the drives present now, the choice and the settings). Blocks a moment
/// (lists the volumes): call it off the UI thread.
pub fn current_plan(state: &AppState) -> Result<RootPlan, String> {
    let volumes = volumes::list_volumes();
    state
        .drive_monitor
        .plan(&state.roots.choice(), &volumes, state.drive_settings.get())
}

/// Brings the indexer to `plan` (offline roots hidden, new or returning roots walked alone,
/// `refresh` walked again). Returns what was queued, if anything.
pub fn apply_plan(app: &AppHandle, plan: &RootPlan, refresh: Vec<PathBuf>) -> Option<StartOutcome> {
    let state = app.state::<AppState>();
    let outcome = state.indexer.reconcile(
        plan.roots.clone(),
        plan.offline.clone(),
        refresh,
        status_listener(app),
    );
    if let Some(outcome) = outcome {
        tracing::info!(?outcome, roots = ?plan.roots, offline = ?plan.offline, "indexed roots updated");
    }
    outcome
}

/// Starts the polling thread (once).
pub fn start_monitor(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.drive_monitor.started.swap(true, Ordering::Relaxed) {
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("athrix-drives".into())
        .spawn(move || {
            loop {
                std::thread::sleep(POLL_INTERVAL);
                if catch_unwind(AssertUnwindSafe(|| poll(&app))).is_err() {
                    tracing::error!("the drive monitor panicked; it keeps polling");
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the drive monitor; plugged drives are not noticed");
    }
}

/// One poll: on a change, tells the UI and brings the index up to date.
fn poll(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some((list, changes)) = state.drive_monitor.check() else {
        return;
    };
    let names = |v: &[Volume]| v.iter().map(|v| v.name.clone()).collect::<Vec<_>>();
    tracing::info!(
        added = ?names(&changes.added),
        removed = ?names(&changes.removed),
        "drives changed"
    );
    if let Err(e) = app.emit(VOLUMES_EVENT, &list) {
        tracing::warn!(error = %e, "cannot emit the volume list");
    }
    let plan =
        match state
            .drive_monitor
            .plan(&state.roots.choice(), &list, state.drive_settings.get())
        {
            Ok(plan) => plan,
            Err(e) => {
                tracing::warn!(error = %e, "cannot work out the roots after a drive change");
                return;
            }
        };
    let refresh = roots_on(&plan.roots, &changes.added);
    apply_plan(app, &plan, refresh);
}

/// The external-drive settings (`get_drive_settings`).
#[tauri::command]
pub fn get_drive_settings(state: State<'_, AppState>) -> DriveSettings {
    state.drive_settings.get()
}

/// Changes the external-drive settings (`set_drive_settings`), saves them, and (in "whole
/// computer" mode) indexes or drops the drives they now take in or leave out. The content of
/// drives left out is deleted, as when a folder is removed from the indexed ones. Returns the
/// saved settings; a save failure is logged and the settings still apply until the app quits.
#[tauri::command]
pub async fn set_drive_settings(
    app: AppHandle,
    settings: DriveSettings,
) -> Result<DriveSettings, String> {
    let state = app.state::<AppState>();
    let before = state.drive_settings.get();
    if let Err(e) = state.drive_settings.set(settings) {
        tracing::warn!(error = %e, "cannot save the drive settings");
    }
    if before != settings && state.roots.choice() == RootsChoice::WholeComputer {
        let handle = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let state = handle.state::<AppState>();
            let old_roots: Vec<PathBuf> = state
                .indexer
                .status()
                .roots
                .into_iter()
                .map(PathBuf::from)
                .collect();
            match current_plan(&state) {
                Ok(plan) => {
                    apply_plan(&handle, &plan, Vec::new());
                    let removed: Vec<PathBuf> = old_roots
                        .into_iter()
                        .filter(|r| !plan.roots.contains(r))
                        .collect();
                    state.content.forget_roots(&removed, &plan.roots);
                }
                Err(e) => tracing::warn!(error = %e, "cannot apply the drive settings"),
            }
        })
        .await
        .map_err(|e| format!("background task failed: {e}"))?;
    }
    Ok(state.drive_settings.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(path: &str, kind: VolumeKind) -> Volume {
        Volume {
            path: path.into(),
            name: path.trim_end_matches('\\').into(),
            label: None,
            kind,
            serial: None,
        }
    }

    #[test]
    fn roots_on_arriving_volumes() {
        let roots = [
            PathBuf::from("C:\\Users"),
            PathBuf::from("E:\\Photos"),
            PathBuf::from("E:\\"),
            PathBuf::from("/media/usb/docs"),
            PathBuf::from("/home/me"),
        ];
        let arrived = [
            vol("E:\\", VolumeKind::Removable),
            vol("/media/usb", VolumeKind::Removable),
            vol("/", VolumeKind::Fixed),
        ];
        assert_eq!(
            roots_on(&roots, &arrived),
            [
                PathBuf::from("E:\\Photos"),
                PathBuf::from("E:\\"),
                PathBuf::from("/media/usb/docs")
            ],
            "`/` never makes everything count as arrived"
        );
        assert!(roots_on(&roots, &[]).is_empty());
    }

    #[test]
    fn the_whole_computer_remembers_external_drives_while_allowed() {
        let monitor = DriveMonitor::default();
        let whole = RootsChoice::WholeComputer;
        let c = vol("C:\\", VolumeKind::Fixed);
        let usb = vol("E:\\", VolumeKind::Removable);
        let on = DriveSettings::default();
        let plugged = monitor.plan(&whole, &[c.clone(), usb], on).unwrap();
        let unplugged = monitor.plan(&whole, std::slice::from_ref(&c), on).unwrap();
        if cfg!(windows) {
            assert_eq!(
                plugged.roots,
                [PathBuf::from("C:\\"), PathBuf::from("E:\\")]
            );
            assert!(plugged.offline.is_empty());
            // Unplugged: still a root, offline (entries hidden, not dropped).
            assert_eq!(unplugged.roots, plugged.roots);
            assert_eq!(unplugged.offline, [PathBuf::from("E:\\")]);
            // Turning the setting off forgets it.
            let off = DriveSettings {
                include_removable: false,
                ..on
            };
            let p = monitor.plan(&whole, std::slice::from_ref(&c), off).unwrap();
            assert_eq!((p.roots, p.offline), (vec![PathBuf::from("C:\\")], vec![]));
            let p = monitor.plan(&whole, std::slice::from_ref(&c), on).unwrap();
            assert!(p.offline.is_empty(), "forgotten for good");
        } else {
            assert_eq!(unplugged.roots, [PathBuf::from("/")]);
        }
        // Chosen roots never use the memory.
        let chosen = RootsChoice::Custom(vec![PathBuf::from("C:\\")]);
        assert!(monitor.plan(&chosen, &[c], on).unwrap().offline.is_empty());
    }
}
