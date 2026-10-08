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
use crate::git::{Commit, Git};
use crate::timeline::{Change, Timeline};
use crate::watch::{self, Batches};

#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// A file differs from its baseline; this is its latest diff.
    Changed(Arc<FileDiff>),
    /// A previously changed file matches its baseline again.
    Reverted(String),
    /// What to show while the session has no changes of its own: the
    /// working tree's uncommitted changes against HEAD, or failing that, the
    /// last commit. Replaces any previous fallback; empty `diffs` means none.
    Fallback {
        label: String,
        diffs: Vec<Arc<FileDiff>>,
    },
    /// The newest commits, newest first. Sent at startup and whenever refs
    /// change.
    Log(Arc<Vec<Commit>>),
    /// A commit's diff against its first parent, as requested with
    /// [`Session::load_commit`].
    Commit {
        hash: String,
        label: String,
        diffs: Vec<Arc<FileDiff>>,
    },
    Error(String),
}

/// Commits listed in [`SessionEvent::Log`].
const LOG_LIMIT: usize = 500;

pub struct Session {
    root: PathBuf,
    mode: BaseMode,
    git: Arc<Git>,
    tx: Sender<SessionEvent>,
    events: Receiver<SessionEvent>,
    timeline: Arc<Mutex<Timeline>>,
    _watcher: RecommendedWatcher,
}

impl Session {
    pub fn start(path: &Path, mode: BaseMode) -> Result<Self> {
        let git = Arc::new(Git::discover(path)?);
        let root = git.root().to_path_buf();
        // Start watching before capturing the baseline so no edit slips
        // between the two.
        let (watcher, batches) = watch::watch(&root)?;
        let (baseline, dirty) = Baseline::capture(&git, mode)?;
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let (tx, events) = mpsc::channel();

        let mut worker = Worker {
            git: git.clone(),
            baseline,
            tx: tx.clone(),
            timeline: timeline.clone(),
            seen: HashMap::new(),
            reported: HashSet::new(),
            seq: 0,
            last_fallback: None,
            last_log: None,
        };
        std::thread::Builder::new()
            .name("changehog-session".into())
            .spawn(move || worker.run(batches, dirty))?;

        Ok(Self {
            root,
            mode,
            git,
            tx,
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

    /// Loads `hash`'s diff in the background; it arrives as a
    /// [`SessionEvent::Commit`] (or an `Error`).
    pub fn load_commit(&self, hash: &str) {
        let (git, tx, hash) = (self.git.clone(), self.tx.clone(), hash.to_string());
        std::thread::spawn(move || {
            let result = commit_diffs(&git, &hash)
                .and_then(|diffs| Ok((format!("commit {}", git.summary(&hash)?), diffs)));
            let _ = tx.send(match result {
                Ok((label, diffs)) => SessionEvent::Commit { hash, label, diffs },
                Err(e) => SessionEvent::Error(format!("commit {hash}: {e}")),
            });
        });
    }
}

/// `commit`'s changes against its first parent (every file, for a root
/// commit).
fn commit_diffs(git: &Git, commit: &str) -> Result<Vec<Arc<FileDiff>>> {
    let parent = git.parent(commit)?;
    let mut diffs = Vec::new();
    for path in git.changed_paths(parent.as_deref(), commit)?.into_iter().take(MAX_FALLBACK_FILES) {
        let base = match &parent {
            Some(parent) => Content::from_blob(git.blob(parent, &path)?),
            None => Content::Absent,
        };
        let current = Content::from_blob(git.blob(commit, &path)?);
        // Passing `current` as the previous version: nothing is "fresh".
        if let Some(d) = diff::compute(&path, &base, Some(&current), &current, 0) {
            diffs.push(Arc::new(d));
        }
    }
    Ok(diffs)
}

struct Worker {
    git: Arc<Git>,
    baseline: Baseline,
    tx: Sender<SessionEvent>,
    timeline: Arc<Mutex<Timeline>>,
    /// Last contents seen per path, for freshness and duplicate suppression.
    seen: HashMap<String, Content>,
    /// Paths for which a `Changed` is currently outstanding.
    reported: HashSet<String>,
    seq: u64,
    /// Label and file stats of the last fallback sent, to skip resending it.
    last_fallback: Option<FallbackSignature>,
    /// Hashes in the last log sent, to skip resending it.
    last_log: Option<Vec<String>>,
}

/// A fallback's label plus (path, added, removed) per file.
type FallbackSignature = (String, Vec<(String, usize, usize)>);

/// Cap on files in a fallback or commit diff (e.g. a huge initial commit).
const MAX_FALLBACK_FILES: usize = 200;

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
        if self.refresh_fallback().is_err() || self.refresh_log().is_err() {
            return;
        }

        while let Some(batch) = batches.next_batch() {
            let git_changed = batch.iter().any(|p| is_git_ref_or_index(self.git.root(), p));
            if git_changed && self.refresh_log().is_err() {
                return;
            }
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
            let had_changes = !self.reported.is_empty();
            for path in paths.iter().filter(|p| !ignored.contains(*p)) {
                if self.process(path, false).is_err() {
                    return;
                }
            }
            // A commit, checkout or staging can change what the fallback shows,
            // and so does the last session change being reverted.
            let now_empty = self.reported.is_empty();
            if now_empty && (git_changed || had_changes) && self.refresh_fallback().is_err() {
                return;
            }
        }
    }

    /// Recomputes the fallback and sends it if it changed.
    fn refresh_fallback(&mut self) -> Result<(), Disconnected> {
        let (label, diffs) = match self.fallback() {
            Ok(fallback) => fallback,
            Err(e) => return self.send(SessionEvent::Error(format!("fallback: {e}"))),
        };
        let signature = (
            label.clone(),
            diffs.iter().map(|d| (d.path.clone(), d.added, d.removed)).collect(),
        );
        if self.last_fallback.as_ref() == Some(&signature) {
            return Ok(());
        }
        self.last_fallback = Some(signature);
        self.send(SessionEvent::Fallback { label, diffs })
    }

    /// Sends the log if it changed.
    fn refresh_log(&mut self) -> Result<(), Disconnected> {
        let commits = match self.git.log(LOG_LIMIT) {
            Ok(commits) => commits,
            Err(e) => return self.send(SessionEvent::Error(format!("git log: {e}"))),
        };
        let hashes: Vec<String> = commits.iter().map(|c| c.hash.clone()).collect();
        if self.last_log.as_ref() == Some(&hashes) {
            return Ok(());
        }
        self.last_log = Some(hashes);
        self.send(SessionEvent::Log(Arc::new(commits)))
    }

    /// Uncommitted changes against HEAD if there are any, otherwise the
    /// last commit against its parent.
    fn fallback(&mut self) -> Result<(String, Vec<Arc<FileDiff>>)> {
        let head = self.git.head_commit()?;
        let dirty = self.git.dirty_paths()?;
        if dirty.is_empty() {
            return match head {
                Some(head) => Ok((
                    format!("last commit {}", self.git.summary(&head)?),
                    commit_diffs(&self.git, &head)?,
                )),
                None => Ok((String::new(), Vec::new())),
            };
        }
        let pairs = dirty
            .into_iter()
            .take(MAX_FALLBACK_FILES)
            .map(|path| {
                let base = match &head {
                    Some(head) => Content::from_blob(self.git.blob(head, &path)?),
                    None => Content::Absent,
                };
                let current = Content::read(&self.git.root().join(&path));
                Ok((path, base, current))
            })
            .collect::<Result<Vec<_>>>()?;
        let label = "uncommitted changes vs HEAD".to_string();

        let mut diffs = Vec::new();
        for (path, base, current) in pairs {
            self.seq += 1;
            // Passing `current` as the previous version: nothing is "fresh".
            if let Some(d) = diff::compute(&path, &base, Some(&current), &current, self.seq) {
                diffs.push(Arc::new(d));
            }
        }
        Ok((label, diffs))
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

/// Whether `path` is git metadata that changes on commit, checkout, reset
/// or staging.
fn is_git_ref_or_index(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root.join(".git")) else {
        return false;
    };
    let first = rel.components().next().and_then(|c| c.as_os_str().to_str());
    matches!(first, Some("HEAD" | "index" | "refs" | "packed-refs"))
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
