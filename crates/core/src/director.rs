//! Decides which file to show and when to flip to the next one.
//!
//! Agents edit in bursts, so changed files are queued and shown one at a time
//! with a minimum dwell. The dwell shrinks as the backlog grows, so playback
//! catches up instead of falling ever further behind. Any user navigation
//! pauses auto-follow; it resumes after a period of inactivity.
//!
//! Time is passed in explicitly so the logic is deterministic and testable.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::diff::FileDiff;
use crate::session::SessionEvent;

#[derive(Clone, Debug)]
pub struct DirectorConfig {
    /// How long to stay on a file when nothing else is waiting.
    pub dwell: Duration,
    /// Lower bound on dwell when the backlog is long.
    pub min_dwell: Duration,
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

pub struct Director {
    cfg: DirectorConfig,
    diffs: HashMap<String, Arc<FileDiff>>,
    /// Changed files in order of first change.
    order: Vec<String>,
    queue: VecDeque<String>,
    current: Option<String>,
    /// When the current file was first shown, and when its dwell last reset.
    shown_at: Instant,
    hold_until: Instant,
    following: bool,
    last_input: Instant,
}

impl Director {
    pub fn new(cfg: DirectorConfig, now: Instant) -> Self {
        Self {
            cfg,
            diffs: HashMap::new(),
            order: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            shown_at: now,
            hold_until: now,
            following: true,
            last_input: now,
        }
    }

    /// Applies a session event. Returns a flip if the visible file changed.
    pub fn apply(&mut self, event: &SessionEvent, now: Instant) -> Option<Flip> {
        match event {
            SessionEvent::Changed(diff) => {
                let path = diff.path.clone();
                if self.diffs.insert(path.clone(), diff.clone()).is_none() {
                    self.order.push(path.clone());
                }
                if self.current.is_none() {
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
                self.diffs.remove(path);
                self.order.retain(|p| p != path);
                self.queue.retain(|p| p != path);
                if self.current.as_ref() == Some(path) {
                    self.current = None;
                    let next = self.queue.pop_front().or_else(|| self.order.last().cloned());
                    return next.map(|next| self.show(next, now));
                }
                None
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
        let next = self.queue.pop_front()?;
        Some(self.show(next, now))
    }

    fn current_dwell(&self) -> Duration {
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

    /// Manually moves `delta` files through the change list (wrapping).
    /// Pauses auto-follow.
    pub fn step(&mut self, delta: isize, now: Instant) -> Option<Flip> {
        self.user_input(now);
        if self.order.is_empty() {
            return None;
        }
        let len = self.order.len() as isize;
        let idx = self
            .current
            .as_ref()
            .and_then(|c| self.order.iter().position(|p| p == c))
            .map_or(0, |i| (i as isize + delta).rem_euclid(len));
        let path = self.order[idx as usize].clone();
        Some(self.show(path, now))
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
        self.current.as_ref().and_then(|p| self.diffs.get(p))
    }

    /// Changed files in order of first change.
    pub fn files(&self) -> impl Iterator<Item = &Arc<FileDiff>> {
        self.order.iter().filter_map(|p| self.diffs.get(p))
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

    fn changed(path: &str) -> SessionEvent {
        SessionEvent::Changed(Arc::new(FileDiff {
            path: path.into(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            body: Body::Text(vec![]),
            focus: None,
            seq: 0,
            at: Instant::now(),
        }))
    }

    fn cur(d: &Director) -> Option<&str> {
        d.current().map(|f| f.path.as_str())
    }

    #[test]
    fn first_change_shows_immediately_then_queues() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        assert!(d.apply(&changed("a"), t0).is_some());
        assert!(d.apply(&changed("b"), t0).is_none());
        assert_eq!(cur(&d), Some("a"));
        assert_eq!(d.queue_len(), 1);

        assert!(d.tick(t0 + Duration::from_millis(500)).is_none());
        let flip = d.tick(t0 + Duration::from_secs(3)).unwrap();
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
        assert!(d.tick(t0 + Duration::from_millis(800)).is_some());
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
        assert!(d.tick(t0 + Duration::from_secs(5)).is_none());
        assert!(d.tick(t0 + Duration::from_secs(21)).is_some());
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
}
