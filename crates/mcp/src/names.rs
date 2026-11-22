//! The app's name index, read from its snapshot (`<app_data>/index/names.snap`).
//!
//! The desktop app saves the snapshot from time to time (atomically: a temporary file, then a
//! rename), so what the server sees can be a few minutes behind the disk; every result is
//! checked against the disk anyway ([`crate::paths::Guard::check`]), so deleted files drop
//! out. The snapshot is loaded on first use and loaded again when its file changes.
//!
//! Searches only ever look at entries under the allowed folders: a mask of those entries is
//! computed once per snapshot and folder set, from the parent links (only entries on the way
//! down to an allowed folder have their path built).

use std::path::Path;
use std::time::SystemTime;

use athrix_fs_index::{EntryId, FileIndex, Query};

use crate::paths::{Guard, key};

/// Folder of the snapshot inside the app data folder.
pub const SNAPSHOT_DIR: &str = "index";
/// File name of the snapshot.
pub const SNAPSHOT_FILE: &str = "names.snap";

/// A loaded snapshot and the mask of its entries under the allowed folders.
#[derive(Debug, Default)]
pub struct Names {
    loaded: Option<Loaded>,
}

#[derive(Debug)]
struct Loaded {
    stamp: (Option<SystemTime>, u64),
    index: FileIndex,
    /// The allowed folders the mask was computed for, and the mask.
    mask: Option<(Vec<String>, Vec<bool>)>,
}

/// Where an entry stands relative to the allowed folders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Unknown,
    /// Neither under an allowed folder nor on the way to one.
    Outside,
    /// On the way down to an allowed folder (its children need a look).
    Ancestor,
    /// An allowed folder or under one.
    Inside,
}

impl Names {
    /// The snapshot in `data_dir`, (re)loaded if its file changed, with the mask for `guard`.
    /// Errors are English messages for the tool result.
    pub fn get(&mut self, data_dir: &Path, guard: &Guard) -> Result<(&FileIndex, &[bool]), String> {
        let file = data_dir.join(SNAPSHOT_DIR).join(SNAPSHOT_FILE);
        let meta = std::fs::metadata(&file).map_err(|_| {
            format!(
                "Athrix has not saved a name index yet ({} is missing): open Athrix and let it \
                 index once",
                file.display()
            )
        })?;
        let stamp = (meta.modified().ok(), meta.len());
        if self.loaded.as_ref().is_none_or(|l| l.stamp != stamp) {
            self.loaded = None;
            let index = FileIndex::load(&file)
                .map_err(|e| format!("cannot read the name index {}: {e}", file.display()))?;
            self.loaded = Some(Loaded {
                stamp,
                index,
                mask: None,
            });
        }
        let loaded = self.loaded.as_mut().expect("loaded above");
        let folders: Vec<String> = guard.folders().map(|p| key(&p.to_string_lossy())).collect();
        if loaded.mask.as_ref().is_none_or(|(f, _)| *f != folders) {
            let mask = mask(&loaded.index, guard);
            loaded.mask = Some((folders, mask));
        }
        let (_, mask) = loaded.mask.as_ref().expect("computed above");
        Ok((&loaded.index, mask))
    }
}

/// `mask[id]`: entry `id` is an allowed folder or lies under one (lexically).
fn mask(index: &FileIndex, guard: &Guard) -> Vec<bool> {
    let n = index.len();
    let mut place = vec![Place::Unknown; n];
    let mut chain: Vec<EntryId> = Vec::new();
    for start in 0..n {
        if place[start] != Place::Unknown {
            continue;
        }
        // Walk up to the first known ancestor (or the root), then settle the chain downwards.
        chain.clear();
        let mut cur = start as EntryId;
        let mut above = None;
        loop {
            chain.push(cur);
            match index.parent(cur) {
                Some(p) if place[p as usize] != Place::Unknown => {
                    above = Some(place[p as usize]);
                    break;
                }
                Some(p) if chain.len() <= n => cur = p,
                _ => break,
            }
        }
        let mut parent_place = above;
        for &id in chain.iter().rev() {
            let p = match parent_place {
                Some(Place::Inside) => Place::Inside,
                Some(Place::Outside) => Place::Outside,
                // A root, or a child of an ancestor: look at its path.
                _ => classify(&index.path(id), guard),
            };
            place[id as usize] = p;
            parent_place = Some(p);
        }
    }
    place.into_iter().map(|p| p == Place::Inside).collect()
}

fn classify(path: &Path, guard: &Guard) -> Place {
    let k = key(&path.to_string_lossy());
    if guard.admits_key(&k) {
        Place::Inside
    } else if guard.is_ancestor_key(&k) {
        Place::Ancestor
    } else {
        Place::Outside
    }
}

/// Up to `limit` entries matching `query` among those set in `mask`: best first for a ranked
/// query, else in index order.
pub fn search(index: &FileIndex, mask: &[bool], query: &Query, limit: usize) -> Vec<EntryId> {
    if limit == 0 {
        return Vec::new();
    }
    let ids = mask
        .iter()
        .enumerate()
        .filter(|(_, m)| **m)
        .map(|(i, _)| i as EntryId);
    if query.is_ranked() {
        let mut scored: Vec<(u32, EntryId)> = ids
            .filter_map(|id| index.score(query, id).map(|s| (s, id)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.truncate(limit);
        scored.into_iter().map(|(_, id)| id).collect()
    } else {
        ids.filter(|&id| index.is_match(query, id))
            .take(limit)
            .collect()
    }
}
