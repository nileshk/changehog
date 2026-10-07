//! Filesystem watching with burst coalescing.
//!
//! Raw events are noisy: one editor save can produce create/modify/rename
//! events for a temp file and its target. We collect paths until the tree has
//! been quiet for `QUIET`, or `MAX_WAIT` has passed since the first event, and
//! hand them over as one batch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const QUIET: Duration = Duration::from_millis(80);
const MAX_WAIT: Duration = Duration::from_millis(400);

pub struct Batches {
    rx: Receiver<notify::Result<notify::Event>>,
}

/// Starts watching `root` recursively. Dropping the returned watcher closes
/// the channel, which ends the batch stream.
pub fn watch(root: &Path) -> Result<(RecommendedWatcher, Batches)> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok((watcher, Batches { rx }))
}

impl Batches {
    /// Blocks until the next batch of changed paths. `None` when the watcher
    /// has shut down.
    pub fn next_batch(&self) -> Option<HashSet<PathBuf>> {
        let mut paths = HashSet::new();
        // Wait for the first relevant event.
        while paths.is_empty() {
            collect(self.rx.recv().ok()?, &mut paths);
        }
        let first = Instant::now();
        loop {
            let remaining = MAX_WAIT.saturating_sub(first.elapsed());
            if remaining.is_zero() {
                return Some(paths);
            }
            match self.rx.recv_timeout(QUIET.min(remaining)) {
                Ok(event) => collect(event, &mut paths),
                Err(RecvTimeoutError::Timeout) => return Some(paths),
                Err(RecvTimeoutError::Disconnected) => return Some(paths),
            }
        }
    }
}

fn collect(event: notify::Result<notify::Event>, paths: &mut HashSet<PathBuf>) {
    let Ok(event) = event else { return };
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    paths.extend(event.paths);
}
