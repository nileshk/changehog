use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use changehog_core::config::LOG_ROWS_RANGE;
use changehog_core::{
    Commit, Config, Director, DirectorConfig, FileDiff, Measure, Session, SessionEvent,
    SidebarMode,
};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};

const FRAME: Duration = Duration::from_millis(33);
/// Redraw at least this often even when idle (for countdowns).
const IDLE_REDRAW: Duration = Duration::from_millis(250);
/// How long fresh lines stay highlighted.
pub const FRESH_FADE: Duration = Duration::from_millis(2500);
/// Below this width the sidebar starts collapsed.
const SIDEBAR_AUTO_WIDTH: u16 = 110;
/// Lines moved per mouse-wheel notch.
const WHEEL_LINES: isize = 3;

/// A clickable region, recorded while drawing.
#[derive(Clone, Debug)]
pub enum Hit {
    File(String),
    ToggleSidebar,
    Commit(String),
    ToggleLog,
    /// The log panel's top edge, which resizes it when dragged.
    LogEdge,
}

pub struct App {
    pub session: Session,
    pub director: Director,
    /// Animated scroll position (in diff lines), easing toward the
    /// director's scroll.
    pub scroll: f32,
    /// Height of the diff viewport, from the last render.
    pub viewport: usize,
    /// Path of the file on screen, to snap (not animate) scroll on flips.
    shown: Option<String>,
    pub message: Option<(String, Instant)>,
    /// Sidebar open/closed as chosen by the user; `None` follows the
    /// terminal width.
    sidebar: Option<bool>,
    /// Width of the body area, from the last render.
    pub body_width: u16,
    /// Clickable regions from the last render.
    pub hits: Vec<(Rect, Hit)>,
    /// Wrap long lines instead of cutting them off.
    pub wrap: bool,
    /// (wrap, text area width) the director's measure was built for.
    measure_key: Option<(bool, usize)>,
    /// Newest commits first.
    pub commits: Arc<Vec<Commit>>,
    pub log_open: bool,
    /// Commits visible in the log panel.
    pub log_rows: u16,
    /// Index of the first commit shown in the log panel.
    pub log_offset: usize,
    /// The log panel's area, from the last render.
    pub log_area: Rect,
    /// Commit whose diff is being loaded.
    pub loading: Option<String>,
    dragging_log: bool,
    quit: bool,
}

impl App {
    pub fn new(session: Session, config: &Config) -> Self {
        let cfg = DirectorConfig {
            cycle_dwell: config.cycle(),
            resume_after: config.resume_after(),
            ..DirectorConfig::default()
        };
        Self {
            session,
            director: Director::new(cfg, Instant::now()),
            scroll: 0.0,
            viewport: 20,
            shown: None,
            message: None,
            sidebar: match config.sidebar {
                SidebarMode::Auto => None,
                SidebarMode::Open => Some(true),
                SidebarMode::Closed => Some(false),
            },
            body_width: 0,
            hits: Vec::new(),
            wrap: config.wrap,
            measure_key: None,
            commits: Arc::new(Vec::new()),
            log_open: config.log,
            log_rows: config.log_rows,
            log_offset: 0,
            log_area: Rect::default(),
            loading: None,
            dragging_log: false,
            quit: false,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut last_draw = Instant::now() - IDLE_REDRAW;
        let mut dirty = true;
        while !self.quit {
            if event::poll(FRAME)? {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) => self.on_key(key),
                        Event::Mouse(mouse) => self.on_mouse(mouse),
                        _ => {}
                    }
                    dirty = true;
                }
            }

            let now = Instant::now();
            dirty |= self.drain_session(now);
            dirty |= self.director.tick(now).is_some();
            dirty |= self.animate(now);

            if dirty || last_draw.elapsed() >= IDLE_REDRAW {
                terminal.draw(|frame| crate::ui::draw(frame, &mut self))?;
                last_draw = Instant::now();
                dirty = false;
            }
        }
        Ok(())
    }

    fn drain_session(&mut self, now: Instant) -> bool {
        let mut changed = false;
        while let Ok(event) = self.session.events().try_recv() {
            match &event {
                SessionEvent::Error(e) => {
                    if self.loading.as_ref().is_some_and(|h| e.starts_with(&format!("commit {h}"))) {
                        self.loading = None;
                    }
                    self.message = Some((e.clone(), now));
                }
                SessionEvent::Log(commits) => {
                    self.commits = commits.clone();
                    self.log_offset = self.log_offset.min(self.commits.len().saturating_sub(1));
                }
                // Results for a commit no longer wanted are ignored.
                SessionEvent::Commit { hash, label, diffs } if self.loading.as_ref() == Some(hash) => {
                    self.loading = None;
                    self.director.pin(hash, label, diffs, now);
                }
                _ => {}
            }
            self.director.apply(&event, now);
            changed = true;
        }
        changed
    }

    /// Eases scroll toward the director's position. Returns true while
    /// anything is animating (scroll or fading highlights).
    fn animate(&mut self, now: Instant) -> bool {
        let target = self.director.scroll() as f32;
        let path = self.director.current().map(|d| &d.path);
        if path != self.shown.as_ref() {
            // A different file: jump straight there rather than scrolling
            // from the previous file's position.
            self.shown = path.cloned();
            self.scroll = target;
        }
        let delta = target - self.scroll;
        let scrolling = delta.abs() > 0.01;
        if scrolling {
            self.scroll = if delta.abs() < 0.5 {
                target
            } else {
                self.scroll + delta * 0.25
            };
        }
        let fading = self
            .director
            .current()
            .is_some_and(|d| now.duration_since(d.at) < FRESH_FADE);
        scrolling || fading
    }

    /// Gives the director a row measure matching the wrap setting and the
    /// diff area's width, when either has changed.
    pub fn sync_measure(&mut self, width: usize) {
        let key = (self.wrap, width);
        if self.measure_key == Some(key) {
            return;
        }
        self.measure_key = Some(key);
        let measure = self
            .wrap
            .then(|| Box::new(move |d: &FileDiff| crate::wrap::line_rows(d, width)) as Measure);
        self.director.set_measure(measure);
        // Row positions changed meaning; don't animate across the change.
        self.scroll = self.director.scroll() as f32;
    }

    pub fn sidebar_open(&self) -> bool {
        self.sidebar.unwrap_or(self.body_width >= SIDEBAR_AUTO_WIDTH)
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar = Some(!self.sidebar_open());
    }

    /// Shows `hash`'s diff, or returns to live changes if it's already shown.
    fn toggle_commit(&mut self, hash: &str, now: Instant) {
        if self.director.pinned() == Some(hash) {
            self.director.unpin(now);
        } else {
            self.loading = Some(hash.to_string());
            self.session.load_commit(hash);
        }
    }

    fn resize_log(&mut self, rows: i32) {
        let (min, max) = (*LOG_ROWS_RANGE.start() as i32, *LOG_ROWS_RANGE.end() as i32);
        self.log_rows = rows.clamp(min, max) as u16;
    }

    fn scroll_log(&mut self, delta: isize) {
        let max = self.commits.len().saturating_sub(self.log_rows as usize);
        self.log_offset = self.log_offset.saturating_add_signed(delta).min(max);
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let now = Instant::now();
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position::new(mouse.column, mouse.row);
                let hit = self.hits.iter().find(|(r, _)| r.contains(pos)).map(|(_, h)| h.clone());
                match hit {
                    Some(Hit::File(path)) => {
                        self.director.select(&path, now);
                    }
                    Some(Hit::ToggleSidebar) => self.toggle_sidebar(),
                    Some(Hit::Commit(hash)) => self.toggle_commit(&hash, now),
                    Some(Hit::ToggleLog) => self.log_open = !self.log_open,
                    Some(Hit::LogEdge) => self.dragging_log = true,
                    None => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_log => {
                // The edge follows the pointer; the panel ends where it did.
                let bottom = (self.log_area.y + self.log_area.height) as i32;
                self.resize_log(bottom - mouse.row as i32 - 1);
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging_log = false,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = mouse.kind == MouseEventKind::ScrollDown;
                let over_log = self.log_open && self.log_area.contains(Position::new(mouse.column, mouse.row));
                match (over_log, down) {
                    (true, true) => self.scroll_log(1),
                    (true, false) => self.scroll_log(-1),
                    (false, true) => self.director.scroll_by(WHEEL_LINES, now),
                    (false, false) => self.director.scroll_by(-WHEEL_LINES, now),
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let now = Instant::now();
        let page = self.viewport.saturating_sub(2).max(1) as isize;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            // Esc backs out of a commit first.
            KeyCode::Esc if self.loading.is_some() => self.loading = None,
            KeyCode::Esc if self.director.pinned().is_some() => {
                self.director.unpin(now);
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.director.scroll_by(1, now),
            KeyCode::Char('k') | KeyCode::Up => self.director.scroll_by(-1, now),
            KeyCode::Char('d') | KeyCode::PageDown | KeyCode::Char(' ') => self.director.scroll_by(page, now),
            KeyCode::Char('u') | KeyCode::PageUp | KeyCode::Char('b') => self.director.scroll_by(-page, now),
            KeyCode::Char('g') | KeyCode::Home => self.director.scroll_by(isize::MIN / 2, now),
            KeyCode::Char('G') | KeyCode::End => self.director.scroll_by(isize::MAX / 2, now),
            KeyCode::Char('n') | KeyCode::Tab | KeyCode::Right => {
                self.director.step(1, now);
            }
            KeyCode::Char('p') | KeyCode::BackTab | KeyCode::Left => {
                self.director.step(-1, now);
            }
            KeyCode::Char('+' | '=' | ']') => {
                self.director.adjust_cycle(true);
            }
            KeyCode::Char('-' | '_' | '[') => {
                self.director.adjust_cycle(false);
            }
            KeyCode::Char('s') => self.toggle_sidebar(),
            KeyCode::Char('w') => self.wrap = !self.wrap,
            KeyCode::Char('l') => self.log_open = !self.log_open,
            KeyCode::Char('{') => self.resize_log(self.log_rows as i32 - 1),
            KeyCode::Char('}') => self.resize_log(self.log_rows as i32 + 1),
            KeyCode::Char('f') => {
                let follow = !self.director.following();
                self.director.set_following(follow, now);
            }
            _ => {}
        }
    }
}
