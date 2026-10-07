//! A watch session: filesystem events in, file diffs out.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::Result;
use notify::RecommendedWatcher;

use crate::baseline::{BaseMode, Baseline, Content};
use crate::diff::{self, FileDiff};
use crate::git::Git;
use crate::timeline::{Change, Timeline};
use crate::watch::{self, Batches};

#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// A file differs from its baseline; this is its latest diff.
    Changed(Arc<FileDiff>),
    /// A previously changed file matches its baseline again.
    Reverted(String),
    Error(String),
}

pub struct Session {
    root: PathBuf,
    mode: BaseMode,
    events: Receiver<SessionEvent>,
    timeline: Arc<Mutex<Timeline>>,
    _watcher: RecommendedWatcher,
}

impl Session {
    pub fn start(path: &Path, mode: BaseMode) -> Result<Self> {
        let git = Git::discover(path)?;
        let root = git.root().to_path_buf();
        // Start watching before capturing the baseline so no edit slips
        // between the two.
        let (watcher, batches) = watch::watch(&root)?;
        let (baseline, dirty) = Baseline::capture(&git, mode)?;
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let (tx, events) = mpsc::channel();

        let mut worker = Worker {
            git,
            baseline,
            tx,
            timeline: timeline.clone(),
            seen: HashMap::new(),
            reported: HashSet::new(),
            seq: 0,
        };
        std::thread::Builder::new()
            .name("diff-live-session".into())
            .spawn(move || worker.run(batches, dirty))?;

        Ok(Self {
            root,
            mode,
            events,
            timeline,
            _watcher: watcher,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mode(&self) -> BaseMode {
        self.mode
    }

    pub fn events(&self) -> &Receiver<SessionEvent> {
        &self.events
    }

    pub fn timeline(&self) -> &Arc<Mutex<Timeline>> {
        &self.timeline
    }
}

struct Worker {
    git: Git,
    baseline: Baseline,
    tx: Sender<SessionEvent>,
    timeline: Arc<Mutex<Timeline>>,
    /// Last contents seen per path, for freshness and duplicate suppression.
    seen: HashMap<String, Content>,
    /// Paths for which a `Changed` is currently outstanding.
    reported: HashSet<String>,
    seq: u64,
}

/// Returned when the frontend has gone away.
struct Disconnected;

impl Worker {
    fn run(&mut self, batches: Batches, dirty_at_start: Vec<String>) {
        // In HEAD mode, pre-existing changes are shown immediately (but not
        // marked fresh). In session mode they're part of the baseline.
        if self.baseline.mode() == BaseMode::Head {
            for path in dirty_at_start {
                if self.process(&path, true).is_err() {
                    return;
                }
            }
        }

        while let Some(batch) = batches.next_batch() {
            let mut paths: Vec<String> = batch
                .iter()
                .filter_map(|p| relative(self.git.root(), p))
                .filter(|rel| !is_noise(rel) && !self.git.root().join(rel).is_dir())
                .collect();
            paths.sort();
            paths.dedup();

            let ignored = match self.git.ignored(&paths) {
                Ok(ignored) => ignored,
                Err(e) => {
                    let _ = self.tx.send(SessionEvent::Error(e.to_string()));
                    HashSet::new()
                }
            };
            for path in paths.iter().filter(|p| !ignored.contains(*p)) {
                if self.process(path, false).is_err() {
                    return;
                }
            }
        }
    }

    fn process(&mut self, path: &str, initial: bool) -> Result<(), Disconnected> {
        let current = Content::read(&self.git.root().join(path));
        if self.seen.get(path) == Some(&current) {
            return Ok(()); // touched, but no content change
        }
        let base = match self.baseline.get(&self.git, path) {
            Ok(base) => base,
            Err(e) => return self.send(SessionEvent::Error(format!("{path}: {e}"))),
        };
        let prev = if initial {
            current.clone()
        } else {
            self.seen.get(path).cloned().unwrap_or_else(|| base.clone())
        };
        self.seen.insert(path.to_string(), current.clone());
        self.seq += 1;

        let event = match diff::compute(path, &base, Some(&prev), &current, self.seq) {
            Some(d) => {
                self.reported.insert(path.to_string());
                self.record(path, d.added, d.removed, current);
                SessionEvent::Changed(Arc::new(d))
            }
            None if self.reported.remove(path) => {
                self.record(path, 0, 0, current);
                SessionEvent::Reverted(path.to_string())
            }
            None => return Ok(()),
        };
        self.send(event)
    }

    fn record(&self, path: &str, added: usize, removed: usize, content: Content) {
        let change = Change {
            seq: self.seq,
            at: SystemTime::now(),
            path: path.to_string(),
            added,
            removed,
        };
        self.timeline.lock().unwrap().record(change, content);
    }

    fn send(&self, event: SessionEvent) -> Result<(), Disconnected> {
        self.tx.send(event).map_err(|_| Disconnected)
    }
}

/// Converts an absolute event path to a `/`-separated repo-relative path,
/// skipping anything inside a `.git` directory.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) if s == ".git" => return None,
            Component::Normal(s) => parts.push(s.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Editor swap files and atomic-save temp files that come and go.
fn is_noise(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == ".DS_Store"
        || name == "4913" // vim's write-permission probe
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".swo")
        || name.ends_with(".tmp")
        || name.contains(".tmp.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        let root = Path::new("/repo");
        assert_eq!(relative(root, Path::new("/repo/src/a.rs")).as_deref(), Some("src/a.rs"));
        assert_eq!(relative(root, Path::new("/repo/.git/index")), None);
        assert_eq!(relative(root, Path::new("/repo/sub/.git/HEAD")), None);
        assert_eq!(relative(root, Path::new("/elsewhere/a.rs")), None);
        assert_eq!(relative(root, Path::new("/repo")), None);
    }

    #[test]
    fn noise() {
        assert!(is_noise("src/.main.rs.swp"));
        assert!(is_noise("src/main.rs.tmp.1234.5678"));
        assert!(is_noise("notes.txt~"));
        assert!(!is_noise("src/main.rs"));
    }
}
