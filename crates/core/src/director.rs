//! Decides which file to show and when to flip to the next one.
//!
//! Agents edit in bursts, so changed files are queued and shown one at a time
//! with a minimum dwell. The dwell shrinks as the backlog grows, so playback
//! catches up instead of falling ever further behind. With nothing queued it
//! keeps cycling through all changed files. When a file's changes don't fit
//! in the viewport, scrolling down to the next off-screen change is a cycle
//! step of its own, before moving on to the next file. Any user navigation
//! pauses auto-follow; it resumes after a period of inactivity.
//!
//! The director owns the scroll position so that frontends only need to
//! report their viewport height and animate toward [`Director::scroll`].
//! Positions are in screen rows: by default each diff line is one row, and a
//! frontend that wraps long lines supplies a [`Measure`] giving the rows each
//! line takes.
//!
//! When the session has no changes of its own, the director shows the
//! session's fallback diffs instead (e.g. the last commit). A commit can also
//! be pinned: its files are shown and cycled through until it's unpinned,
//! while live changes are recorded in the background.
//!
//! Time is passed in explicitly so the logic is deterministic and testable.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::diff::{DiffLine, FileDiff, LineKind};
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

/// Cycle dwell presets, fastest first, stepped through by
/// [`Director::adjust_cycle`].
pub const CYCLE_STEPS: [Duration; 10] = [
    Duration::from_millis(1000),
    Duration::from_millis(1500),
    Duration::from_millis(2000),
    Duration::from_millis(3000),
    Duration::from_millis(4000),
    Duration::from_millis(6000),
    Duration::from_millis(8000),
    Duration::from_millis(12000),
    Duration::from_millis(20000),
    Duration::from_millis(30000),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flip {
    pub from: Option<String>,
    pub to: String,
}

/// One step of auto-follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    /// Moved to another file.
    Flip(Flip),
    /// Scrolled within the current file to bring more changes on screen.
    Scroll { from: usize, to: usize },
    /// A commit pinned with [`Director::play`] has had one full pass: every
    /// file and scroll step. Nothing moved; it's the caller's cue to go on.
    /// Repeats each dwell until the caller pins something else.
    EndOfPass,
}

/// Where to scroll when a file is shown.
#[derive(Clone, Copy)]
enum At {
    /// The most interesting change (the latest edit, if any).
    Focus,
    /// The first hunk.
    Top,
}

/// Returns how many screen rows each of a diff's lines takes.
pub type Measure = Box<dyn Fn(&FileDiff) -> Vec<usize>>;

/// Rows of context kept above a change when scrolling to it.
const SCROLL_MARGIN: usize = 4;

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

/// A commit being viewed instead of live changes.
struct Pinned {
    id: String,
    label: String,
    files: FileSet,
    /// Report [`Transition::EndOfPass`] instead of cycling back to the start.
    playback: bool,
}

pub struct Director {
    cfg: DirectorConfig,
    pinned: Option<Pinned>,
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
    /// Top row of the view, and the view's height, in screen rows.
    scroll: usize,
    viewport: usize,
    measure: Option<Measure>,
    /// First row of each line of the current diff, plus the total row count
    /// as a final entry.
    row_starts: Vec<usize>,
}

impl Director {
    pub fn new(cfg: DirectorConfig, now: Instant) -> Self {
        Self {
            cfg,
            pinned: None,
            live: FileSet::default(),
            fallback: FileSet::default(),
            fallback_label: None,
            queue: VecDeque::new(),
            current: None,
            shown_at: now,
            hold_until: now,
            following: true,
            last_input: now,
            scroll: 0,
            viewport: 20,
            measure: None,
            row_starts: vec![0],
        }
    }

    /// The set being displayed: a pinned commit, live changes, or the
    /// fallback when there are none.
    fn active(&self) -> &FileSet {
        if let Some(pinned) = &self.pinned {
            &pinned.files
        } else if self.live.is_empty() {
            &self.fallback
        } else {
            &self.live
        }
    }

    /// Applies a session event. Returns a flip if the visible file changed.
    pub fn apply(&mut self, event: &SessionEvent, now: Instant) -> Option<Flip> {
        match event {
            SessionEvent::Changed(diff) if self.pinned.is_some() => {
                // Recorded and queued for when the commit is unpinned.
                self.live.insert(diff.clone());
                if !self.queue.contains(&diff.path) {
                    self.queue.push_back(diff.path.clone());
                }
                None
            }
            SessionEvent::Reverted(path) if self.pinned.is_some() => {
                self.live.remove(path);
                self.queue.retain(|p| p != path);
                None
            }
            SessionEvent::Fallback { label, diffs } if self.pinned.is_some() => {
                self.set_fallback(label, diffs);
                None
            }
            SessionEvent::Changed(diff) => {
                let path = diff.path.clone();
                self.live.insert(diff.clone());
                if self.current.as_ref() == Some(&path) {
                    self.relayout();
                }
                let showing_live = self.current.as_ref().is_some_and(|c| self.live.contains(c));
                if !showing_live {
                    // Nothing shown, or only the fallback: jump straight to it.
                    return Some(self.show(path, now, At::Focus));
                }
                if self.current.as_ref() == Some(&path) {
                    // Keep the file up while it's being actively edited, but
                    // not forever if others are waiting.
                    self.shown_at = now.min(self.hold_until);
                    self.scroll = if self.following {
                        self.focus_scroll()
                    } else {
                        self.clamp_scroll(self.scroll)
                    };
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
                    self.relayout();
                    let next = self.queue.pop_front().or_else(|| self.active().step_from(None, 0));
                    return next.map(|next| self.show(next, now, At::Focus));
                }
                None
            }
            SessionEvent::Fallback { label, diffs } => {
                self.set_fallback(label, diffs);
                let current_ok = self
                    .current
                    .as_ref()
                    .is_some_and(|c| self.live.contains(c) || self.fallback.contains(c));
                if current_ok {
                    // Its diff may have been replaced.
                    self.relayout();
                    self.scroll = self.clamp_scroll(self.scroll);
                    return None;
                }
                self.current = None;
                self.relayout();
                let next = self.active().step_from(None, 0);
                next.map(|next| self.show(next, now, At::Top))
            }
            SessionEvent::Log(_) | SessionEvent::Commit { .. } | SessionEvent::Error(_) => None,
        }
    }

    fn set_fallback(&mut self, label: &str, diffs: &[Arc<FileDiff>]) {
        self.fallback = FileSet::default();
        for diff in diffs {
            self.fallback.insert(diff.clone());
        }
        self.fallback_label = (!diffs.is_empty()).then(|| label.to_string());
    }

    /// Shows a commit's files instead of live changes until [`unpin`] is
    /// called. Live changes keep being recorded and queued meanwhile.
    /// Pauses auto-follow, like other navigation.
    ///
    /// [`unpin`]: Director::unpin
    pub fn pin(&mut self, id: &str, label: &str, diffs: &[Arc<FileDiff>], now: Instant) -> Option<Flip> {
        self.user_input(now);
        self.pin_files(id, label, diffs, false, now)
    }

    /// Pins a commit for playback: like [`Director::pin`], but auto-follow
    /// stays on, and after one full pass `tick` reports
    /// [`Transition::EndOfPass`] instead of starting over.
    pub fn play(&mut self, id: &str, label: &str, diffs: &[Arc<FileDiff>], now: Instant) -> Option<Flip> {
        self.following = true;
        self.last_input = now;
        self.pin_files(id, label, diffs, true, now)
    }

    fn pin_files(
        &mut self,
        id: &str,
        label: &str,
        diffs: &[Arc<FileDiff>],
        playback: bool,
        now: Instant,
    ) -> Option<Flip> {
        // The dwell for a commit with no files starts now.
        self.shown_at = now;
        let mut files = FileSet::default();
        for diff in diffs {
            files.insert(diff.clone());
        }
        self.pinned = Some(Pinned {
            id: id.to_string(),
            label: label.to_string(),
            files,
            playback,
        });
        self.current = None;
        self.relayout();
        let first = self.active().step_from(None, 0)?;
        Some(self.show(first, now, At::Top))
    }

    /// Returns to live changes (or the fallback) and resumes auto-follow.
    pub fn unpin(&mut self, now: Instant) -> Option<Flip> {
        self.pinned.take()?;
        self.following = true;
        self.last_input = now;
        self.current = None;
        self.relayout();
        let next = self.queue.pop_front().or_else(|| self.active().step_from(None, 0))?;
        Some(self.show(next, now, At::Focus))
    }

    /// The id passed to [`Director::pin`], while a commit is pinned.
    pub fn pinned(&self) -> Option<&str> {
        self.pinned.as_ref().map(|p| p.id.as_str())
    }

    /// The label passed to [`Director::pin`], while a commit is pinned.
    pub fn pinned_label(&self) -> Option<&str> {
        self.pinned.as_ref().map(|p| p.label.as_str())
    }

    /// Advances time. Returns a transition when it's time for the next step:
    /// a queued file, the next off-screen change in this file, or the next
    /// file in the cycle.
    pub fn tick(&mut self, now: Instant) -> Option<Transition> {
        if !self.following && now.duration_since(self.last_input) >= self.cfg.resume_after {
            self.following = true;
        }
        if !self.following || now.duration_since(self.shown_at) < self.current_dwell() {
            return None;
        }
        if self.pinned.is_none()
            && let Some(next) = self.queue.pop_front()
        {
            return Some(Transition::Flip(self.show(next, now, At::Focus)));
        }
        if let Some(to) = self.next_stop() {
            return Some(self.scroll_step(to, now));
        }
        let active = self.active();
        let at_last = match &self.current {
            Some(c) => active.order.last() == Some(c),
            None => active.is_empty(),
        };
        if at_last && self.pinned.as_ref().is_some_and(|p| p.playback) {
            self.shown_at = now;
            return Some(Transition::EndOfPass);
        }
        if active.order.len() < 2 && self.current.is_some() {
            // Only one file: start over from its top if we scrolled down.
            return (self.scroll > 0).then(|| self.scroll_step(0, now));
        }
        let next = active.step_from(self.current.as_deref(), 1)?;
        Some(Transition::Flip(self.show(next, now, At::Top)))
    }

    fn scroll_step(&mut self, to: usize, now: Instant) -> Transition {
        let from = std::mem::replace(&mut self.scroll, to);
        self.shown_at = now;
        Transition::Scroll { from, to }
    }

    /// Scroll position that brings the next change that isn't fully on
    /// screen into view, if there is one.
    fn next_stop(&self) -> Option<usize> {
        let bottom = self.scroll + self.viewport;
        let margin = SCROLL_MARGIN.min(self.viewport / 4);
        let (i, _) = self.lines().iter().enumerate().find(|&(i, l)| {
            matches!(l.kind, LineKind::Added | LineKind::Removed) && self.line_row(i + 1) > bottom
        })?;
        let start = self.line_row(i);
        // A line that starts below the view is brought to the top; one that's
        // already partly visible (a long wrapped line) is paged through.
        let target = if start >= bottom { start } else { bottom }.saturating_sub(margin);
        let to = self.clamp_scroll(target.max(self.scroll + 1));
        (to > self.scroll).then_some(to)
    }

    fn lines(&self) -> &[DiffLine] {
        self.current().map_or(&[], |d| d.lines())
    }

    /// First row of line `i` (the total row count for one past the end).
    fn line_row(&self, i: usize) -> usize {
        let total = self.total_rows();
        self.row_starts.get(i).copied().unwrap_or(total)
    }

    fn total_rows(&self) -> usize {
        self.row_starts.last().copied().unwrap_or(0)
    }

    fn clamp_scroll(&self, scroll: usize) -> usize {
        scroll.min(self.total_rows().saturating_sub(self.viewport))
    }

    /// Puts the current file's focus line a third of the way down the view.
    fn focus_scroll(&self) -> usize {
        let focus = self.current().and_then(|d| d.focus).unwrap_or(0);
        self.clamp_scroll(self.line_row(focus).saturating_sub(self.viewport / 3))
    }

    /// Recomputes row positions for the current diff.
    fn relayout(&mut self) {
        let heights = match self.current() {
            None => Vec::new(),
            Some(diff) => {
                let n = diff.lines().len();
                self.measure
                    .as_ref()
                    .map(|m| m(diff))
                    .filter(|h| h.len() == n)
                    .unwrap_or_else(|| vec![1; n])
            }
        };
        let mut starts = Vec::with_capacity(heights.len() + 1);
        let mut row = 0;
        starts.push(0);
        for h in heights {
            row += h.max(1);
            starts.push(row);
        }
        self.row_starts = starts;
    }

    /// The top row of the view.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// First row of each line of the current diff, followed by the total
    /// row count.
    pub fn row_starts(&self) -> &[usize] {
        &self.row_starts
    }

    /// The line containing `row`, and how many rows into it `row` is.
    pub fn line_at_row(&self, row: usize) -> (usize, usize) {
        let line = self.row_starts.partition_point(|&r| r <= row).saturating_sub(1);
        (line, row - self.line_row(line).min(row))
    }

    /// Tells the director how many rows fit on screen.
    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        self.scroll = self.clamp_scroll(self.scroll);
    }

    /// Sets how many rows each line takes (`None`: one row per line). Keeps
    /// the line at the top of the view in place.
    pub fn set_measure(&mut self, measure: Option<Measure>) {
        let (top_line, _) = self.line_at_row(self.scroll);
        self.measure = measure;
        self.relayout();
        self.scroll = self.clamp_scroll(self.line_row(top_line));
    }

    /// Manually scrolls by `delta` rows. Pauses auto-follow.
    pub fn scroll_by(&mut self, delta: isize, now: Instant) {
        self.user_input(now);
        self.scroll = self.clamp_scroll(self.scroll.saturating_add_signed(delta));
    }

    fn current_dwell(&self) -> Duration {
        if self.queue.is_empty() || self.pinned.is_some() {
            return self.cfg.cycle_dwell;
        }
        // Never linger on queued files longer than when idly cycling.
        let base = self.cfg.dwell.min(self.cfg.cycle_dwell);
        let factor = 1.0 + self.queue.len() as f32 * 0.5;
        base.div_f32(factor).max(self.cfg.min_dwell.min(base))
    }

    /// How long each file is shown when cycling.
    pub fn cycle_dwell(&self) -> Duration {
        self.cfg.cycle_dwell
    }

    /// Steps the cycle dwell to the next faster or slower preset and
    /// returns the new value.
    pub fn adjust_cycle(&mut self, faster: bool) -> Duration {
        let cur = self.cfg.cycle_dwell;
        let next = if faster {
            CYCLE_STEPS.iter().rev().find(|&&d| d < cur).unwrap_or(&CYCLE_STEPS[0])
        } else {
            CYCLE_STEPS.iter().find(|&&d| d > cur).unwrap_or(&CYCLE_STEPS[CYCLE_STEPS.len() - 1])
        };
        self.cfg.cycle_dwell = *next;
        *next
    }

    fn show(&mut self, path: String, now: Instant, at: At) -> Flip {
        if self.pinned.is_none() {
            self.queue.retain(|p| p != &path);
        }
        self.shown_at = now;
        self.hold_until = now + self.cfg.max_hold;
        let from = self.current.replace(path.clone());
        self.relayout();
        self.scroll = match at {
            At::Focus => self.focus_scroll(),
            At::Top => 0,
        };
        Flip { from, to: path }
    }

    /// Manually moves `delta` files through the list (wrapping). Pauses
    /// auto-follow.
    pub fn step(&mut self, delta: isize, now: Instant) -> Option<Flip> {
        self.user_input(now);
        let next = self.active().step_from(self.current.as_deref(), delta)?;
        Some(self.show(next, now, At::Focus))
    }

    /// Manually shows `path`, if it's in the displayed set. Pauses
    /// auto-follow.
    pub fn select(&mut self, path: &str, now: Instant) -> Option<Flip> {
        self.user_input(now);
        if !self.active().contains(path) {
            return None;
        }
        Some(self.show(path.to_string(), now, At::Focus))
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
            // Back to the latest change; let the next tick flip immediately
            // if anything is waiting.
            self.scroll = self.focus_scroll();
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
        match &self.pinned {
            Some(pinned) => pinned.files.diffs.get(path),
            None => self.live.diffs.get(path).or_else(|| self.fallback.diffs.get(path)),
        }
    }

    /// The files being displayed: session changes in order of first change,
    /// or the fallback set when there are none.
    pub fn files(&self) -> impl Iterator<Item = &Arc<FileDiff>> {
        let active = self.active();
        active.order.iter().filter_map(|p| active.diffs.get(p))
    }

    /// Describes the fallback when it's what's being shown.
    pub fn fallback_label(&self) -> Option<&str> {
        if self.live.is_empty() && self.pinned.is_none() {
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

    /// A diff whose lines follow `shape`: `h` hunk header, `c` context,
    /// `+` added, `-` removed.
    fn shaped(path: &str, shape: &str) -> Arc<FileDiff> {
        let lines = shape
            .chars()
            .map(|c| DiffLine {
                kind: match c {
                    'h' => LineKind::HunkHeader,
                    '+' => LineKind::Added,
                    '-' => LineKind::Removed,
                    _ => LineKind::Context,
                },
                old_no: None,
                new_no: None,
                text: String::new(),
                fresh: false,
            })
            .collect();
        Arc::new(FileDiff {
            path: path.into(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            body: Body::Text(lines),
            focus: Some(1),
            seq: 0,
            at: Instant::now(),
        })
    }

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
        assert_eq!(flip, Transition::Flip(Flip { from: Some("a".into()), to: "b".into() }));
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
    fn cycle_speed_adjusts_through_presets() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        assert_eq!(d.adjust_cycle(true), secs(3.0));
        assert_eq!(d.adjust_cycle(false), secs(4.0));
        for _ in 0..20 {
            d.adjust_cycle(true);
        }
        assert_eq!(d.cycle_dwell(), CYCLE_STEPS[0]);

        // A custom value snaps to the neighbouring preset.
        let mut d = Director::new(DirectorConfig { cycle_dwell: secs(5.0), ..Default::default() }, t0);
        assert_eq!(d.adjust_cycle(false), secs(6.0));

        // Faster cycling applies immediately, and caps the queued dwell too.
        let mut d = Director::new(DirectorConfig { cycle_dwell: secs(1.0), ..Default::default() }, t0);
        for p in ["a", "b", "c"] {
            d.apply(&changed(p), t0);
        }
        assert!(d.tick(t0 + secs(1.0)).is_some());
    }

    #[test]
    fn scrolls_to_offscreen_changes_before_flipping() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.set_viewport(10);
        // Changes at 1, 15 and 32; 40 lines in all.
        let shape = format!("h+{}+{}+{}", "c".repeat(13), "c".repeat(16), "c".repeat(7));
        d.apply(&SessionEvent::Changed(shaped("long", &shape)), t0);
        d.apply(&changed("short"), t0);
        d.tick(t0 + secs(3.0)); // queued "short" first
        assert_eq!(cur(&d), Some("short"));

        let mut t = t0 + secs(3.0);
        let mut step = || {
            t += secs(4.0);
            d.tick(t).unwrap()
        };
        assert!(matches!(step(), Transition::Flip(f) if f.to == "long"));
        assert_eq!(step(), Transition::Scroll { from: 0, to: 13 }, "line 15, 2 lines of margin");
        assert_eq!(step(), Transition::Scroll { from: 13, to: 30 }, "clamped to the end");
        assert!(matches!(step(), Transition::Flip(f) if f.to == "short"));
    }

    #[test]
    fn single_long_file_scrolls_then_wraps() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.set_viewport(10);
        d.apply(&SessionEvent::Changed(shaped("long", &format!("h+{}+", "c".repeat(20)))), t0);
        assert_eq!(d.scroll(), 0);
        assert!(matches!(d.tick(t0 + secs(4.0)), Some(Transition::Scroll { to: 13, .. })));
        assert_eq!(d.tick(t0 + secs(8.0)), Some(Transition::Scroll { from: 13, to: 0 }));
    }

    /// Each line takes `rows` rows, except lines whose shape char is `+`,
    /// which take `tall`.
    fn measure(tall: usize) -> Measure {
        Box::new(move |d: &FileDiff| {
            d.lines()
                .iter()
                .map(|l| if l.kind == LineKind::Added { tall } else { 1 })
                .collect()
        })
    }

    #[test]
    fn pages_through_a_change_taller_than_the_view() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.set_viewport(10);
        d.set_measure(Some(measure(25)));
        d.apply(&SessionEvent::Changed(shaped("long", "h+cc")), t0);
        assert_eq!(d.row_starts(), &[0, 1, 26, 27, 28]);
        assert_eq!(d.scroll(), 0);
        let mut t = t0;
        let mut step = || {
            t += secs(4.0);
            d.tick(t)
        };
        // Pages keep 2 rows (a quarter of the view) of overlap.
        assert_eq!(step(), Some(Transition::Scroll { from: 0, to: 8 }));
        assert_eq!(step(), Some(Transition::Scroll { from: 8, to: 16 }));
        // The change now ends at the bottom of the view; only context is left.
        assert_eq!(step(), Some(Transition::Scroll { from: 16, to: 0 }), "wraps to the top");
    }

    #[test]
    fn wrapped_rows_count_toward_offscreen_changes() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.set_viewport(10);
        // Two changes 6 lines apart: they fit on screen unwrapped...
        let shape = format!("h+{}+", "c".repeat(5));
        d.apply(&SessionEvent::Changed(shaped("f", &shape)), t0);
        assert_eq!(d.tick(t0 + secs(4.0)), None);
        // ...but not when each changed line wraps to 3 rows.
        d.set_measure(Some(measure(3)));
        assert_eq!(d.tick(t0 + secs(8.0)), Some(Transition::Scroll { from: 0, to: 2 }));
    }

    #[test]
    fn changing_the_measure_keeps_the_top_line() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.set_viewport(5);
        d.apply(&SessionEvent::Changed(shaped("f", &"+".repeat(20))), t0);
        d.scroll_by(6, t0);
        assert_eq!(d.line_at_row(d.scroll()), (6, 0));
        d.set_measure(Some(measure(2)));
        assert_eq!(d.scroll(), 12);
        assert_eq!(d.line_at_row(13), (6, 1));
        d.set_measure(None);
        assert_eq!(d.scroll(), 6);
    }

    #[test]
    fn manual_scroll_pauses_and_clamps() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.set_viewport(10);
        d.apply(&SessionEvent::Changed(shaped("long", &"+".repeat(25))), t0);
        d.scroll_by(100, t0);
        assert_eq!(d.scroll(), 15);
        assert!(!d.following());
        d.scroll_by(-100, t0);
        assert_eq!(d.scroll(), 0);
    }

    #[test]
    fn select_jumps_to_a_file_and_pauses() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        for p in ["a", "b", "c"] {
            d.apply(&changed(p), t0);
        }
        assert_eq!(d.select("c", t0).map(|f| f.to), Some("c".into()));
        assert_eq!(cur(&d), Some("c"));
        assert!(!d.following());
        assert!(d.select("nope", t0).is_none());
        assert_eq!(cur(&d), Some("c"));
    }

    #[test]
    fn pinned_commit_holds_until_unpinned() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.apply(&changed("live"), t0);
        d.pin("abc", "commit abc", &[diff("x"), diff("y")], t0);
        assert_eq!((cur(&d), d.pinned()), (Some("x"), Some("abc")));
        assert_eq!(d.files().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["x", "y"]);

        // Live changes don't take over, even once auto-follow resumes...
        d.apply(&changed("new"), t0);
        d.apply(&SessionEvent::Changed(shaped("x", "+++")), t0);
        let t = t0 + secs(21.0);
        assert!(matches!(d.tick(t), Some(Transition::Flip(f)) if f.to == "y"), "cycles the commit");
        assert!(matches!(d.tick(t + secs(4.0)), Some(Transition::Flip(f)) if f.to == "x"));
        assert_eq!(d.current().unwrap().lines().len(), 0, "the commit's x, not the live one");

        // ...and are shown once it's unpinned, queued ones first.
        d.unpin(t + secs(5.0));
        assert_eq!((cur(&d), d.pinned()), (Some("new"), None));
        assert!(d.following());
        assert_eq!(d.files().count(), 3);
    }

    #[test]
    fn playback_reports_the_end_of_each_pass() {
        let t0 = Instant::now();
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.set_viewport(10);
        let long = shaped("long", &format!("h+{}+", "c".repeat(20)));
        d.play("c1", "commit c1", &[diff("a"), long], t0);
        assert!(d.following(), "playback doesn't pause auto-follow");
        let mut t = t0;
        let mut step = || {
            t += secs(4.0);
            d.tick(t)
        };
        assert!(matches!(step(), Some(Transition::Flip(f)) if f.to == "long"));
        assert!(matches!(step(), Some(Transition::Scroll { .. })), "scroll steps still happen");
        assert_eq!(step(), Some(Transition::EndOfPass));
        assert_eq!(step(), Some(Transition::EndOfPass), "repeats each dwell, nothing moves");
        assert_eq!(cur(&d), Some("long"));

        // An empty commit (e.g. a merge) ends after one dwell.
        d.play("c2", "commit c2", &[], t);
        assert_eq!(d.tick(t + secs(1.0)), None);
        assert_eq!(d.tick(t + secs(4.0)), Some(Transition::EndOfPass));

        // A plain pin still loops.
        let mut d = Director::new(DirectorConfig::default(), t0);
        d.pin("c1", "commit c1", &[diff("a"), diff("b")], t0);
        d.set_following(true, t0);
        assert!(matches!(d.tick(t0 + secs(4.0)), Some(Transition::Flip(_))));
        assert!(matches!(d.tick(t0 + secs(8.0)), Some(Transition::Flip(f)) if f.to == "a"));
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
