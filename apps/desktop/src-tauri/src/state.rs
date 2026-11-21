//! State managed by Tauri and shared by all commands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::indexing::Indexer;
use crate::preview::PreviewAssets;
use crate::roots::RootsSetting;

/// Everything the commands share: the indexer, the indexed-roots setting, the cancel flags of
/// the latest search and preview, and the images the preview may serve.
#[derive(Debug, Default)]
pub struct AppState {
    /// The name index and its background indexing.
    pub indexer: Arc<Indexer>,
    /// Which folders to index (persisted in the app data folder).
    pub roots: RootsSetting,
    /// Cancel flag of the most recent `search` request.
    search_cancel: Mutex<Arc<AtomicBool>>,
    /// Cancel flag of the most recent `preview_entry` request.
    preview_cancel: Mutex<Arc<AtomicBool>>,
    /// Held while a preview is built: previews run one at a time, so a burst of requests
    /// (holding an arrow key) never extracts many documents at once.
    pub preview_gate: Arc<Mutex<()>>,
    /// Images the `athrix-preview` scheme may serve.
    pub preview_assets: Arc<PreviewAssets>,
}

impl AppState {
    /// Cancels the previous search (sets its flag) and returns a fresh flag for a new one.
    pub fn begin_search(&self) -> Arc<AtomicBool> {
        replace_flag(&self.search_cancel)
    }

    /// Cancels the previous preview and returns a fresh flag for a new one.
    pub fn begin_preview(&self) -> Arc<AtomicBool> {
        replace_flag(&self.preview_cancel)
    }
}

/// Sets the flag in `slot` and puts a fresh one there instead.
fn replace_flag(slot: &Mutex<Arc<AtomicBool>>) -> Arc<AtomicBool> {
    let fresh = Arc::new(AtomicBool::new(false));
    let previous = std::mem::replace(
        &mut *slot.lock().unwrap_or_else(PoisonError::into_inner),
        Arc::clone(&fresh),
    );
    previous.store(true, Ordering::Relaxed);
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_search_cancels_the_previous_one() {
        let state = AppState::default();
        let first = state.begin_search();
        assert!(!first.load(Ordering::Relaxed));
        let second = state.begin_search();
        assert!(first.load(Ordering::Relaxed));
        assert!(!second.load(Ordering::Relaxed));
        // Previews have their own flag.
        let preview = state.begin_preview();
        assert!(!second.load(Ordering::Relaxed));
        state.begin_preview();
        assert!(preview.load(Ordering::Relaxed));
    }
}
