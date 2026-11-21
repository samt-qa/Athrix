//! State managed by Tauri and shared by all commands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::indexing::Indexer;
use crate::roots::RootsSetting;

/// Everything the commands share: the indexer, the indexed-roots setting and the cancel flag
/// of the latest search.
#[derive(Debug, Default)]
pub struct AppState {
    /// The name index and its background indexing.
    pub indexer: Arc<Indexer>,
    /// Which folders to index (persisted in the app data folder).
    pub roots: RootsSetting,
    /// Cancel flag of the most recent `search` request.
    search_cancel: Mutex<Arc<AtomicBool>>,
}

impl AppState {
    /// Cancels the previous search (sets its flag) and returns a fresh flag for a new one.
    pub fn begin_search(&self) -> Arc<AtomicBool> {
        let fresh = Arc::new(AtomicBool::new(false));
        let previous = std::mem::replace(
            &mut *self
                .search_cancel
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Arc::clone(&fresh),
        );
        previous.store(true, Ordering::Relaxed);
        fresh
    }
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
    }
}
