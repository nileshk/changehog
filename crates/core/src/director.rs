//! Decides which file to show and when to flip to the next one.
//!
//! Agents edit in bursts, so changed files are queued and shown one at a time
//! with a minimum dwell. The dwell shrinks as the backlog grows, so playback
//! catches up instead of falling ever further behind. With nothing queued it
//! keeps cycling through all changed files. Any user navigation pauses
//! auto-follow; it resumes after a period of inactivity.
//!
//! When the session has no changes of its own, the director shows the
//! session's fallback diffs instead (e.g. the last commit).
//!
//! Time is passed in explicitly so the logic is deterministic and testable.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::diff::FileDiff;
use crate::session::SessionEvent;

#[derive(Clone, Debug)]
pub struct DirectorConfig {
    /// How long to stay on a file when others are waiting.
    pub dwell: Duration,
    /// Lower bound on dwell when the backlog is long.
    pub min_dwell: Duration,
    /// How long to stay on each file when idly cycling.
    pub cycle_dwell: Duration,
    /// How long continued edits to the current file can hold off others.
    pub max_hold: Duration,
    /// Inactivity after which auto-follow resumes.
    pub resume_after: Duration,
}

impl Default for DirectorConfig {
    fn default() -> Self {
        Self {
            dwell: Duration::from_millis(2500),
            min_dwell: Duration::from_millis(600),
            cycle_dwell: Duration::from_secs(4),
            max_hold: Duration::from_secs(8),
            resume_after: Duration::from_secs(20),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flip {
    pub from: Option<String>,
    pub to: String,
}

/// Diffs keyed by path, plus their display order.
#[derive(Default)]
struct FileSet {
    diffs: HashMap<String, Arc<FileDiff>>,
    order: Vec<String>,
}

impl FileSet {
    fn insert(&mut self, diff: Arc<FileDiff>) {
        let path = diff.path.clone();
        if self.diffs.insert(path.clone(), diff).is_none() {
            self.order.push(path);
        }
    }

    fn remove(&mut self, path: &str) {
        self.diffs.remove(path);
        self.order.retain(|p| p != path);
    }

    fn contains(&self, path: &str) -> bool {
        self.diffs.contains_key(path)
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The path `delta` places after `from` (wrapping), or the first path if
    /// `from` isn't in the set.
    fn step_from(&self, from: Option<&str>, delta: isize) -> Option<String> {
        let len = self.order.len() as isize;
        if len == 0 {
            return None;
        }
        let idx = from
            .and_then(|c| self.order.iter().position(|p| p == c))
            .map_or(0, |i| (i as isize + delta).rem_euclid(len));
        Some(self.order[idx as usize].clone())
    }
}

pub struct Director {
    cfg: DirectorConfig,
    /// Files changed during the session, in order of first change.
    live: FileSet,
    /// Shown while `live` is empty.
    fallback: FileSet,
    fallback_label: Option<String>,
    queue: VecDeque<String>,
    current: Option<String>,
    /// When the current file's dwell started, and the latest it can be
    /// extended to by further edits.
    shown_at: Instant,
    hold_until: Instant,
    following: bool,
    last_input: Instant,
}

impl Director {
    pub fn new(cfg: DirectorConfig, now: Instant) -> Self {
        Self {
            cfg,
            live: FileSet::default(),
            fallback: FileSet::default(),
            fallback_label: None,
            queue: VecDeque::new(),
            current: None,
            shown_at: now,
            hold_until: now,
            following: true,
            last_input: now,
        }
    }

    /// The set being displayed: live changes, or the fallback when there are none.
    fn active(&self) -> &FileSet {
        if self.live.is_empty() {
            &self.fallback
        } else {
            &self.live
        }
    }

    /// Applies a session event. Returns a flip if the visible file changed.
    pub fn apply(&mut self, event: &SessionEvent, now: Instant) -> Option<Flip> {
        match event {
            SessionEvent::Changed(diff) => {
                let path = diff.path.clone();
                self.live.insert(diff.clone());
                let showing_live = self.current.as_ref().is_some_and(|c| self.live.contains(c));
                if !showing_live {
                    // Nothing shown, or only the fallback: jump straight to it.
                    return Some(self.show(path, now));
                }
                if self.current.as_ref() == Some(&path) {
                    // Keep the file up while it's being actively edited, but
                    // not forever if others are waiting.
                    self.shown_at = now.min(self.hold_until);
                } else if !self.queue.contains(&path) {
                    self.queue.push_back(path);
                }
                None
            }
            SessionEvent::Reverted(path) => {
                self.live.remove(path);
                self.queue.retain(|p| p != path);
                if self.current.as_ref() == Some(path) {
                    self.current = None;
                    let next = self.queue.pop_front().or_else(|| self.active().step_from(None, 0));
                    return next.map(|next| self.show(next, now));
                }
                None
            }
            SessionEvent::Fallback { label, diffs } => {
                self.fallback = FileSet::default();
                for diff in diffs {
                    self.fallback.insert(diff.clone());
                }
                self.fallback_label = (!diffs.is_empty()).then(|| label.clone());
                let current_ok = self
                    .current
                    .as_ref()
                    .is_some_and(|c| self.live.contains(c) || self.fallback.contains(c));
                if current_ok {
                    return None;
                }
                self.current = None;
                let next = self.active().step_from(None, 0);
                next.map(|next| self.show(next, now))
            }
            SessionEvent::Error(_) => None,
        }
    }

    /// Advances time. Returns a flip when it's time to move to the next file.
    pub fn tick(&mut self, now: Instant) -> Option<Flip> {
        if !self.following && now.duration_since(self.last_input) >= self.cfg.resume_after {
            self.following = true;
        }
        if !self.following || now.duration_since(self.shown_at) < self.current_dwell() {
            return None;
        }
        let next = match self.queue.pop_front() {
            Some(next) => next,
            None => {
                let active = self.active();
                if active.order.len() < 2 && self.current.is_some() {
                    return None;
                }
                active.step_from(self.current.as_deref(), 1)?
            }
        };
        Some(self.show(next, now))
    }

    fn current_dwell(&self) -> Duration {
        if self.queue.is_empty() {
            return self.cfg.cycle_dwell;
        }
        let factor = 1.0 + self.queue.len() as f32 * 0.5;
        self.cfg.dwell.div_f32(factor).max(self.cfg.min_dwell)
    }

    fn show(&mut self, path: String, now: Instant) -> Flip {
        self.queue.retain(|p| p != &path);
        self.shown_at = now;
        self.hold_until = now + self.cfg.max_hold;
        Flip {
            from: self.current.replace(path.clone()),
            to: path,
        }
    }

    /// Manually moves `delta` files through the list (wrapping). Pauses
    /// auto-follow.
    pub fn step(&mut self, delta: isize, now: Instant) -> Option<Flip> {
        self.user_input(now);
        let next = self.active().step_from(self.current.as_deref(), delta)?;
        Some(self.show(next, now))
    }

    /// Records user activity, pausing auto-follow.
    pub fn user_input(&mut self, now: Instant) {
        self.following = false;
        self.last_input = now;
    }

    pub fn set_following(&mut self, following: bool, now: Instant) {
        self.following = following;
        self.last_input = now;
        if following {
            // Let the next tick flip immediately if anything is waiting.
            self.shown_at = now.checked_sub(self.cfg.dwell).unwrap_or(now);
        }
    }

    pub fn following(&self) -> bool {
        self.following
    }

    /// Time until auto-follow resumes, when paused.
    pub fn resumes_in(&self, now: Instant) -> Option<Duration> {
        (!self.following)
            .then(|| self.cfg.resume_after.saturating_sub(now.duration_since(self.last_input)))
    }

    pub fn current(&self) -> Option<&Arc<FileDiff>> {
        let path = self.current.as_ref()?;
        self.live.diffs.get(path).or_else(|| self.fallback.diffs.get(path))
    }

    /// The files being displayed: session changes in order of first change,
    /// or the fallback set when there are none.
    pub fn files(&self) -> impl Iterator<Item = &Arc<FileDiff>> {
        let active = self.active();
        active.order.iter().filter_map(|p| active.diffs.get(p))
    }

    /// Describes the fallback when it's what's being shown.
    pub fn fallback_label(&self) -> Option<&str> {
        if self.live.is_empty() {
            self.fallback_label.as_deref()
        } else {
            None
        }
    }

    pub fn is_queued(&self, path: &str) -> bool {
        self.queue.iter().any(|p| p == path)
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Body, FileStatus};

    fn diff(path: &str) -> Arc<FileDiff> {
        Arc::new(FileDiff {
            path: path.into(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            body: Body::Text(vec![]),
            focus: None,
            seq: 0,
            at: Instant::now(),
        })
    }

    fn changed(path: &str) -> SessionEvent {
        SessionEvent::Changed(diff(path))
    }

    fn fallback(paths: &[&str]) -> SessionEvent {
        SessionEvent::Fallback {
            label: "last commit".into(),
            diffs: paths.iter().map(|p| diff(p)).collect(),
        }
    }

    fn cur(d: &Director) -> Option<&str> {
        d.current().map(|f| f.path.as_str())
    }

    fn secs(s: f32) -> Duration {
        Duration::from_secs_f32(s)
    }

    #[test]
    fn first_change_shows_immediately_then_queues() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        assert!(d.apply(&changed("a"), t0).is_some());
        assert!(d.apply(&changed("b"), t0).is_none());
        assert_eq!(cur(&d), Some("a"));
        assert_eq!(d.queue_len(), 1);

        assert!(d.tick(t0 + secs(0.5)).is_none());
        let flip = d.tick(t0 + secs(3.0)).unwrap();
        assert_eq!(flip, Flip { from: Some("a".into()), to: "b".into() });
    }

    #[test]
    fn backlog_shortens_dwell() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        for p in ["a", "b", "c", "d", "e", "f"] {
            d.apply(&changed(p), t0);
        }
        // 5 queued: dwell = 2.5s / 3.5 ≈ 714ms.
        assert!(d.tick(t0 + secs(0.8)).is_some());
    }

    #[test]
    fn keeps_cycling_once_the_queue_drains() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        for p in ["a", "b", "c"] {
            d.apply(&changed(p), t0);
        }
        let mut t = t0;
        let mut seen = vec![cur(&d).unwrap().to_string()];
        for _ in 0..6 {
            t += secs(4.0);
            d.tick(t).expect("should keep flipping");
            seen.push(cur(&d).unwrap().to_string());
        }
        assert_eq!(seen, ["a", "b", "c", "a", "b", "c", "a"]);
    }

    #[test]
    fn single_file_does_not_flip() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.apply(&changed("a"), t0);
        assert!(d.tick(t0 + secs(10.0)).is_none());
    }

    #[test]
    fn manual_navigation_pauses_then_resumes() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.apply(&changed("a"), t0);
        d.apply(&changed("b"), t0);
        d.step(1, t0);
        assert_eq!(cur(&d), Some("b"));
        assert!(!d.following());
        d.apply(&changed("a"), t0);
        assert!(d.tick(t0 + secs(5.0)).is_none());
        assert!(d.tick(t0 + secs(21.0)).is_some());
        assert_eq!(cur(&d), Some("a"));
    }

    #[test]
    fn revert_of_current_moves_on() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.apply(&changed("a"), t0);
        d.apply(&changed("b"), t0);
        let flip = d.apply(&SessionEvent::Reverted("a".into()), t0).unwrap();
        assert_eq!(flip.to, "b");
        assert_eq!(d.files().count(), 1);
        assert!(d.apply(&SessionEvent::Reverted("b".into()), t0).is_none());
        assert_eq!(cur(&d), None);
    }

    #[test]
    fn fallback_shows_until_live_changes_arrive() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.apply(&fallback(&["x", "y"]), t0);
        assert_eq!(cur(&d), Some("x"));
        assert_eq!(d.fallback_label(), Some("last commit"));
        d.tick(t0 + secs(4.0));
        assert_eq!(cur(&d), Some("y"), "fallback cycles too");

        // A live change takes over immediately.
        d.apply(&changed("a"), t0 + secs(4.5));
        assert_eq!(cur(&d), Some("a"));
        assert_eq!(d.fallback_label(), None);
        assert_eq!(d.files().count(), 1);

        // Reverting it falls back again.
        d.apply(&SessionEvent::Reverted("a".into()), t0 + secs(5.0));
        assert_eq!(cur(&d), Some("x"));
    }
}
