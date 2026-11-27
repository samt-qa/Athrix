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
//! their entries and plugging them back walks that drive alone. The memory survives restarts
//! ([`KNOWN_DRIVES_FILE`] in the app data folder: each drive's root with its kind, serial
//! number and label), so after a restart an unplugged stick is still listed as offline ("not
//! connected: E:"). A remembered drive that shows up under another letter (same serial) is
//! forgotten under the old one; at most [`MAX_KNOWN_DRIVES`] are kept.
//!
//! **Safe removal.** Roots on removable drives are polled rather than watched by the OS (see
//! [`crate::indexing::POLL_INTERVAL`]), so Athrix keeps no handle open on a USB stick and
//! Windows' "safely remove" is not refused because of it. External USB / FireWire disks that
//! Windows calls "fixed" are found by their bus and listed as removable
//! ([`crate::volumes`]), so they are polled, remembered offline and governed by the
//! removable-drives setting like sticks.
//!
//! Commands: [`get_drive_settings`] / [`set_drive_settings`] (persisted next to
//! `indexed-roots.json`, see [`crate::roots::DRIVE_SETTINGS_FILE`]).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
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

/// File name of the remembered external drives inside the app data folder.
pub const KNOWN_DRIVES_FILE: &str = "known-drives.json";

const KNOWN_DRIVES_VERSION: u32 = 1;

/// At most this many external drives are remembered (the least recently seen are forgotten).
pub const MAX_KNOWN_DRIVES: usize = 32;

/// What the monitor knows (in [`AppState`]).
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
    /// External drives "the whole computer" indexed, most recently seen last. Persisted in
    /// `file` (if set) so an unplugged drive is still shown offline after a restart.
    remembered: Vec<KnownDrive>,
    file: Option<PathBuf>,
}

/// An external drive "the whole computer" indexed, as remembered across restarts
/// ([`KNOWN_DRIVES_FILE`]): its root and its identity (kind, serial number, label) when last
/// seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownDrive {
    /// The drive root, e.g. `E:\`.
    pub root: PathBuf,
    /// Removable (sticks, cards, external disks) or network.
    pub kind: VolumeKind,
    /// The volume serial number, if known: the same drive seen under another letter replaces
    /// this entry.
    #[serde(default)]
    pub serial: Option<u32>,
    /// The volume label, if any (for logs and diagnostics).
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct KnownDrivesFile {
    version: u32,
    drives: Vec<KnownDrive>,
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

    /// Points the memory of external drives at `file` and loads it (drives remembered this
    /// session so far are kept). Only the first call has an effect; returns true if it loaded
    /// anything. A missing, corrupt or other-version file means nothing remembered (logged).
    pub fn load_from(&self, file: PathBuf) -> bool {
        let mut m = self.lock();
        if m.file.is_some() {
            return false;
        }
        let loaded = read_known_drives(&file);
        let any = !loaded.is_empty();
        let session = std::mem::take(&mut m.remembered);
        m.remembered = loaded;
        for d in session {
            remember(&mut m.remembered, d);
        }
        m.file = Some(file);
        any
    }

    /// The external drives remembered now, least recently seen first.
    pub fn known_drives(&self) -> Vec<KnownDrive> {
        self.lock().remembered.clone()
    }

    /// The plan of `choice` with `volumes` and `settings`, remembering the external drives it
    /// indexes (whole computer only) and keeping the remembered ones the settings still take
    /// in (offline while absent). A remembered drive whose serial number now shows under
    /// another letter is forgotten under the old one. Changes are saved (see
    /// [`DriveMonitor::load_from`]); a save failure is logged.
    pub fn plan(
        &self,
        choice: &RootsChoice,
        volumes: &[Volume],
        settings: DriveSettings,
    ) -> Result<RootPlan, String> {
        let mut m = self.lock();
        let before = m.remembered.clone();
        m.remembered.retain(|d| settings.includes(d.kind));
        // The same drive under another letter: forget the old letter.
        m.remembered.retain(|d| {
            !volumes.iter().any(|v| {
                v.serial.is_some()
                    && v.serial == d.serial
                    && v.kind == d.kind
                    && path_key(Path::new(&v.path)) != path_key(&d.root)
            })
        });
        let remembered: Vec<PathBuf> = match choice {
            RootsChoice::WholeComputer => m.remembered.iter().map(|d| d.root.clone()).collect(),
            RootsChoice::Custom(_) => Vec::new(),
        };
        let plan = roots::plan(choice, volumes, settings, &remembered)?;
        if *choice == RootsChoice::WholeComputer {
            for v in volumes {
                let root = PathBuf::from(&v.path);
                if v.kind != VolumeKind::Fixed && plan.roots.contains(&root) {
                    let drive = KnownDrive {
                        root,
                        kind: v.kind,
                        serial: v.serial,
                        label: v.label.clone(),
                    };
                    remember(&mut m.remembered, drive);
                }
            }
        }
        if m.remembered != before {
            if let Some(file) = &m.file {
                if let Err(e) = write_known_drives(file, &m.remembered) {
                    tracing::warn!(error = %e, "cannot save the remembered external drives");
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

/// Records `drive` in `known` as the most recently seen: an entry for the same root is
/// replaced (another stick may now use that letter), and the oldest entries go beyond
/// [`MAX_KNOWN_DRIVES`]. Only reorders when `drive` is already the latest and unchanged.
fn remember(known: &mut Vec<KnownDrive>, drive: KnownDrive) {
    if known.last() == Some(&drive) {
        return;
    }
    known.retain(|d| path_key(&d.root) != path_key(&drive.root));
    known.push(drive);
    if known.len() > MAX_KNOWN_DRIVES {
        let extra = known.len() - MAX_KNOWN_DRIVES;
        known.drain(..extra);
    }
}

fn read_known_drives(file: &Path) -> Vec<KnownDrive> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the remembered external drives");
            return Vec::new();
        }
    };
    match serde_json::from_slice::<KnownDrivesFile>(&bytes) {
        Ok(f) if f.version == KNOWN_DRIVES_VERSION => {
            let mut drives = Vec::new();
            for d in f.drives {
                // Only drive roots ever get remembered; anything else is a damaged file.
                if volumes::is_drive_root(&d.root.to_string_lossy()) && d.kind != VolumeKind::Fixed
                {
                    remember(&mut drives, d);
                }
            }
            drives
        }
        Ok(f) => {
            tracing::warn!(
                version = f.version,
                "remembered external drives have another version; ignoring them"
            );
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "remembered external drives are corrupt; ignoring them");
            Vec::new()
        }
    }
}

fn write_known_drives(file: &Path, drives: &[KnownDrive]) -> std::io::Result<()> {
    let body = KnownDrivesFile {
        version: KNOWN_DRIVES_VERSION,
        drives: drives.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&body).map_err(std::io::Error::other)?;
    roots::write_atomic(file, &json)
}

/// Loads the remembered external drives from `<app_data>/known-drives.json` (if not done
/// yet) and, when that makes drives of the current plan offline that the indexer does not
/// know as such, hides them: after a restart an unplugged USB stick is shown as "not
/// connected" again, and is walked alone when it comes back.
fn load_known_drives(app: &AppHandle) {
    let state = app.state::<AppState>();
    let dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(error = %e, "no app data folder; external drives are remembered for this session only");
            return;
        }
    };
    if !state.drive_monitor.load_from(dir.join(KNOWN_DRIVES_FILE)) {
        return;
    }
    let volumes = state.drive_monitor.lock().volumes.clone();
    match state
        .drive_monitor
        .plan(&state.roots.choice(), &volumes, state.drive_settings.get())
    {
        Ok(plan) => {
            let mut current = state.indexer.offline_roots();
            let mut wanted = plan.offline.clone();
            current.sort();
            wanted.sort();
            if current != wanted {
                tracing::info!(offline = ?plan.offline, "remembered external drives not connected");
                state.indexer.set_offline_roots(&plan.offline);
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot work out the roots of the remembered drives"),
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

/// Loads the remembered external drives ([`KNOWN_DRIVES_FILE`]; those unplugged now are
/// marked offline) and starts the polling thread (once).
pub fn start_monitor(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.drive_monitor.started.swap(true, Ordering::Relaxed) {
        return;
    }
    load_known_drives(app);
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
    if before.include_onedrive != settings.include_onedrive {
        let handle = app.clone();
        let dropped = before.include_onedrive;
        tauri::async_runtime::spawn_blocking(move || {
            let state = handle.state::<AppState>();
            let roots: Vec<PathBuf> = state
                .indexer
                .status()
                .roots
                .into_iter()
                .map(PathBuf::from)
                .collect();
            if dropped {
                state.content.forget_roots(&roots::onedrive_folders(), &[]);
            }
            // The exclusions changed: walk everything again with them.
            if !roots.is_empty() {
                let outcome = state.indexer.start(roots, status_listener(&handle));
                tracing::info!(?outcome, dropped, "OneDrive setting changed; re-indexing");
            }
        })
        .await
        .map_err(|e| format!("background task failed: {e}"))?;
    }
    let drives_changed = before.include_removable != settings.include_removable
        || before.include_network != settings.include_network;
    if drives_changed && state.roots.choice() == RootsChoice::WholeComputer {
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

    fn usb(path: &str, serial: u32, label: &str) -> Volume {
        Volume {
            serial: Some(serial),
            label: Some(label.into()),
            ..vol(path, VolumeKind::Removable)
        }
    }

    #[test]
    fn remembered_drives_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub").join(KNOWN_DRIVES_FILE);
        let whole = RootsChoice::WholeComputer;
        let on = DriveSettings::default();
        let c = vol("C:\\", VolumeKind::Fixed);
        let stick = usb("E:\\", 0xA, "Photos");

        // First session: the stick is indexed (and saved) while plugged in.
        let first = DriveMonitor::default();
        assert!(!first.load_from(file.clone()), "nothing remembered yet");
        first.plan(&whole, &[c.clone(), stick.clone()], on).unwrap();
        if !cfg!(windows) {
            // Elsewhere external drives live under `/` and are never remembered.
            assert!(first.known_drives().is_empty());
            return;
        }
        assert_eq!(
            first.known_drives(),
            [KnownDrive {
                root: PathBuf::from("E:\\"),
                kind: VolumeKind::Removable,
                serial: Some(0xA),
                label: Some("Photos".into()),
            }]
        );

        // Restart with the stick unplugged: still a root, offline.
        let second = DriveMonitor::default();
        assert!(second.load_from(file.clone()));
        assert!(!second.load_from(file.clone()), "loaded once");
        let plan = second.plan(&whole, std::slice::from_ref(&c), on).unwrap();
        assert_eq!(plan.roots, [PathBuf::from("C:\\"), PathBuf::from("E:\\")]);
        assert_eq!(plan.offline, [PathBuf::from("E:\\")]);

        // The same stick under another letter: the old letter is forgotten (and saved).
        let moved = usb("F:\\", 0xA, "Photos");
        let plan = second.plan(&whole, &[c.clone(), moved], on).unwrap();
        assert_eq!(plan.roots, [PathBuf::from("C:\\"), PathBuf::from("F:\\")]);
        assert!(plan.offline.is_empty());
        let third = DriveMonitor::default();
        third.load_from(file.clone());
        assert_eq!(
            third
                .known_drives()
                .iter()
                .map(|d| d.root.clone())
                .collect::<Vec<_>>(),
            [PathBuf::from("F:\\")]
        );

        // Turning removable drives off forgets them for good.
        let off = DriveSettings {
            include_removable: false,
            ..on
        };
        third.plan(&whole, std::slice::from_ref(&c), off).unwrap();
        let fourth = DriveMonitor::default();
        assert!(!fourth.load_from(file));
    }

    #[test]
    fn the_memory_is_bounded_and_damaged_files_are_ignored() {
        let mut known = Vec::new();
        for i in 0..(MAX_KNOWN_DRIVES + 5) {
            let letter = (b'D' + (i % 20) as u8) as char;
            remember(
                &mut known,
                KnownDrive {
                    root: PathBuf::from(format!("{letter}:\\")),
                    kind: VolumeKind::Removable,
                    serial: Some(i as u32),
                    label: None,
                },
            );
        }
        assert_eq!(known.len(), 20, "one entry per root");
        assert_eq!(
            known.last().unwrap().serial,
            Some(MAX_KNOWN_DRIVES as u32 + 4)
        );

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(KNOWN_DRIVES_FILE);
        for body in [
            "not json",
            r#"{"version":99,"drives":[]}"#,
            r#"{"version":1,"drives":[{"root":"C:\\Users","kind":"removable"},{"root":"D:\\","kind":"fixed"}]}"#,
        ] {
            std::fs::write(&file, body).unwrap();
            assert!(read_known_drives(&file).is_empty(), "{body}");
        }
        std::fs::write(
            &file,
            r#"{"version":1,"drives":[{"root":"E:\\","kind":"removable"}]}"#,
        )
        .unwrap();
        let read = read_known_drives(&file);
        assert_eq!(read.len(), 1);
        assert_eq!((read[0].serial, read[0].label.as_deref()), (None, None));
    }
}
