use std::time::{Duration, Instant};

use anyhow::Result;
use diff_live_core::{Director, DirectorConfig, Session, SessionEvent};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const FRAME: Duration = Duration::from_millis(33);
/// Redraw at least this often even when idle (for countdowns).
const IDLE_REDRAW: Duration = Duration::from_millis(250);
/// How long fresh lines stay highlighted.
pub const FRESH_FADE: Duration = Duration::from_millis(2500);

pub struct App {
    pub session: Session,
    pub director: Director,
    /// Animated scroll position (in diff lines) and where it's heading.
    pub scroll: f32,
    pub target: usize,
    /// Height of the diff viewport, from the last render.
    pub viewport: usize,
    /// (path, seq) of the diff currently on screen, to detect updates.
    shown: Option<(String, u64)>,
    pub message: Option<(String, Instant)>,
    quit: bool,
}

impl App {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            director: Director::new(DirectorConfig::default(), Instant::now()),
            scroll: 0.0,
            target: 0,
            viewport: 20,
            shown: None,
            message: None,
            quit: false,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut last_draw = Instant::now() - IDLE_REDRAW;
        let mut dirty = true;
        while !self.quit {
            if event::poll(FRAME)? {
                while event::poll(Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        self.on_key(key);
                    }
                    dirty = true;
                }
            }

            let now = Instant::now();
            dirty |= self.drain_session(now);
            dirty |= self.director.tick(now).is_some();
            self.sync_scroll();
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
            if let SessionEvent::Error(e) = &event {
                self.message = Some((e.clone(), now));
            }
            self.director.apply(&event, now);
            changed = true;
        }
        changed
    }

    /// Repositions the scroll target when the visible file flips or updates.
    fn sync_scroll(&mut self) {
        let Some(diff) = self.director.current() else {
            self.shown = None;
            return;
        };
        let key = (diff.path.clone(), diff.seq);
        if self.shown.as_ref() == Some(&key) {
            return;
        }
        let flipped = self.shown.as_ref().is_none_or(|(p, _)| *p != diff.path);
        // While paused, keep the user's scroll position on in-place updates.
        if flipped || self.director.following() {
            let focus = diff.focus.unwrap_or(0);
            self.target = self.clamp(focus.saturating_sub(self.viewport / 3));
            if flipped {
                self.scroll = self.target as f32;
            }
        }
        self.shown = Some(key);
    }

    /// Eases scroll toward its target. Returns true while anything is
    /// animating (scroll or fading highlights).
    fn animate(&mut self, now: Instant) -> bool {
        let delta = self.target as f32 - self.scroll;
        let scrolling = delta.abs() > 0.01;
        if scrolling {
            self.scroll = if delta.abs() < 0.5 {
                self.target as f32
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

    fn clamp(&self, line: usize) -> usize {
        let len = self.director.current().map_or(0, |d| d.lines().len());
        line.min(len.saturating_sub(self.viewport.max(1)))
    }

    fn scroll_by(&mut self, delta: isize, now: Instant) {
        self.director.user_input(now);
        self.target = self.clamp(self.target.saturating_add_signed(delta));
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let now = Instant::now();
        let page = self.viewport.saturating_sub(2).max(1) as isize;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1, now),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1, now),
            KeyCode::Char('d') | KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page, now),
            KeyCode::Char('u') | KeyCode::PageUp | KeyCode::Char('b') => self.scroll_by(-page, now),
            KeyCode::Char('g') | KeyCode::Home => self.scroll_by(isize::MIN / 2, now),
            KeyCode::Char('G') | KeyCode::End => self.scroll_by(isize::MAX / 2, now),
            KeyCode::Char('n') | KeyCode::Tab | KeyCode::Right => {
                self.director.step(1, now);
            }
            KeyCode::Char('p') | KeyCode::BackTab | KeyCode::Left => {
                self.director.step(-1, now);
            }
            KeyCode::Char('f') => {
                let follow = !self.director.following();
                self.director.set_following(follow, now);
                if follow {
                    self.shown = None; // re-focus the latest change
                }
            }
            _ => {}
        }
    }
}
