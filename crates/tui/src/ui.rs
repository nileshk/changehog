use std::time::Instant;

use changehog_core::{BaseMode, Body, DiffLine, FileDiff, FileStatus, LineKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::app::{App, FRESH_FADE, Hit};
use crate::wrap;

const SIDEBAR_WIDTH: u16 = 36;
/// Width of the strip shown when the sidebar is collapsed.
const COLLAPSED_WIDTH: u16 = 2;

mod palette {
    use ratatui::style::Color;

    pub const DIM: Color = Color::Rgb(110, 116, 128);
    pub const ACCENT: Color = Color::Rgb(122, 162, 247);
    pub const HUNK: Color = Color::Rgb(110, 150, 220);
    pub const ADD_FG: Color = Color::Rgb(170, 230, 170);
    pub const ADD_BG: Color = Color::Rgb(18, 46, 26);
    pub const ADD_FRESH: Color = Color::Rgb(48, 128, 64);
    pub const DEL_FG: Color = Color::Rgb(240, 170, 170);
    pub const DEL_BG: Color = Color::Rgb(56, 20, 24);
    pub const DEL_FRESH: Color = Color::Rgb(150, 46, 54);
    pub const FRESH_MARK: Color = Color::Rgb(240, 200, 90);
    pub const PAUSED: Color = Color::Rgb(240, 180, 90);
    pub const ERROR: Color = Color::Rgb(240, 110, 110);
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    draw_footer(frame, footer, app);

    app.hits.clear();
    app.body_width = body.width;
    let side_width = if app.sidebar_open() {
        SIDEBAR_WIDTH.min(body.width / 2)
    } else {
        COLLAPSED_WIDTH
    };
    let [side, main] =
        Layout::horizontal([Constraint::Length(side_width), Constraint::Min(1)]).areas(body);
    if app.sidebar_open() {
        draw_sidebar(frame, side, app);
    } else {
        draw_collapsed_sidebar(frame, side, app);
    }
    draw_diff(frame, main, app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let now = Instant::now();
    let repo = app
        .session
        .root()
        .file_name()
        .map_or_else(|| app.session.root().display().to_string(), |n| n.to_string_lossy().into_owned());
    let base = match (app.director.fallback_label(), app.session.mode()) {
        (Some(label), _) => label.to_string(),
        (None, BaseMode::SessionStart) => "since session start".to_string(),
        (None, BaseMode::Head) => "since HEAD".to_string(),
    };
    let follow = match app.director.resumes_in(now) {
        None => Span::styled("● following", Style::new().fg(Color::Green)),
        Some(d) => Span::styled(
            format!("‖ paused ({}s)", d.as_secs() + 1),
            Style::new().fg(palette::PAUSED),
        ),
    };
    let cycle = Span::styled(
        format!("⟳ {}", format_secs(app.director.cycle_dwell())),
        Style::new().fg(palette::DIM),
    );
    let files = app.director.files().count();
    let queued = app.director.queue_len();
    let sep = || Span::styled("  │  ", Style::new().fg(palette::DIM));
    let mut spans = vec![
        Span::styled(" changehog ", Style::new().fg(Color::Black).bg(palette::ACCENT).bold()),
        Span::raw(" "),
        Span::styled(repo, Style::new().bold()),
        sep(),
        Span::styled(base, Style::new().fg(palette::DIM)),
        sep(),
        follow,
        Span::raw(" "),
        cycle,
        Span::styled(if app.wrap { "  ↩ wrap" } else { "" }, Style::new().fg(palette::DIM)),
        sep(),
        Span::raw(format!("{files} file{}", if files == 1 { "" } else { "s" })),
    ];
    if queued > 0 {
        spans.push(Span::styled(format!(", {queued} queued"), Style::new().fg(palette::PAUSED)));
    }
    frame.render_widget(Line::from(spans), area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    if let Some((msg, at)) = &app.message
        && at.elapsed().as_secs() < 8
    {
        frame.render_widget(
            Line::styled(format!(" {msg}"), Style::new().fg(palette::ERROR)),
            area,
        );
        return;
    }
    let keys = [
        ("j/k", "scroll"),
        ("d/u", "page"),
        ("n/p", "file"),
        ("f", "follow"),
        ("+/-", "speed"),
        ("s", "sidebar"),
        ("w", "wrap"),
        ("q", "quit"),
    ];
    let spans: Vec<Span> = keys
        .iter()
        .flat_map(|(k, v)| {
            [
                Span::styled(format!(" {k}"), Style::new().fg(palette::ACCENT)),
                Span::styled(format!(" {v} "), Style::new().fg(palette::DIM)),
            ]
        })
        .collect();
    frame.render_widget(Line::from(spans), area);
}

/// A thin strip with an expand button; clicking anywhere on it expands.
fn draw_collapsed_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::new().fg(palette::DIM));
    frame.render_widget(block, area);
    frame.render_widget(Span::styled("»", Style::new().fg(palette::ACCENT).bold()), area);
    app.hits.push((area, Hit::ToggleSidebar));
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::new().fg(palette::DIM))
        .title(Span::styled(
            if app.director.fallback_label().is_some() { " Files " } else { " Changed " },
            Style::new().fg(palette::DIM),
        ))
        .title_top(Line::from(Span::styled("« ", Style::new().fg(palette::ACCENT).bold())).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // The title row collapses the sidebar.
    app.hits.push((Rect { height: 1, ..area }, Hit::ToggleSidebar));

    let current = app.director.current().map(|d| d.path.clone());
    let current = current.as_deref();
    let width = inner.width as usize;
    let rows = inner.height as usize;
    let files: Vec<_> = app.director.files().cloned().collect();
    // Keep the current file in view when the list is longer than the panel.
    let current_idx = files.iter().position(|d| Some(d.path.as_str()) == current).unwrap_or(0);
    let offset = (current_idx + 1).saturating_sub(rows);
    for (row, d) in files.iter().skip(offset).take(rows).enumerate() {
        let rect = Rect {
            y: inner.y + row as u16,
            height: 1,
            ..inner
        };
        app.hits.push((rect, Hit::File(d.path.clone())));
    }
    let lines: Vec<Line> = files
        .iter()
        .skip(offset)
        .take(rows)
        .map(|d| {
            let is_current = Some(d.path.as_str()) == current;
            let marker = if is_current {
                "▶ "
            } else if app.director.is_queued(&d.path) {
                "• "
            } else {
                "  "
            };
            let stats = format!(" +{} -{}", d.added, d.removed);
            let path_width = width.saturating_sub(marker.chars().count() + stats.len() + 2);
            let mut style = Style::new();
            if is_current {
                style = style.bg(Color::Rgb(40, 44, 56)).bold();
            }
            Line::from(vec![
                Span::styled(marker, Style::new().fg(palette::ACCENT)),
                Span::styled(status_char(d.status), status_style(d.status)),
                Span::raw(" "),
                Span::raw(truncate_left(&d.path, path_width)),
                Span::styled(format!(" +{}", d.added), Style::new().fg(palette::ADD_FG)),
                Span::styled(format!(" -{}", d.removed), Style::new().fg(palette::DEL_FG)),
            ])
            .style(style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_diff(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(diff) = app.director.current().cloned() else {
        let msg = format!("Watching {} — waiting for changes…", app.session.root().display());
        let [_, mid, _] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)])
            .areas(area);
        frame.render_widget(
            Paragraph::new(msg).style(Style::new().fg(palette::DIM)).centered(),
            mid,
        );
        app.viewport = area.height as usize;
        app.director.set_viewport(app.viewport);
        return;
    };

    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(status_char(diff.status), status_style(diff.status)),
        Span::raw(" "),
        Span::styled(diff.path.clone(), Style::new().bold()),
        Span::styled(format!("  +{}", diff.added), Style::new().fg(palette::ADD_FG)),
        Span::styled(format!(" -{} ", diff.removed), Style::new().fg(palette::DEL_FG)),
    ]);
    let block = Block::new()
        .borders(Borders::TOP)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(palette::DIM))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.viewport = inner.height as usize;
    app.director.set_viewport(app.viewport);
    app.sync_measure(inner.width as usize);

    let lines = match &diff.body {
        Body::Text(lines) => lines,
        Body::Binary => return placeholder(frame, inner, "Binary file changed"),
        Body::TooLarge => return placeholder(frame, inner, "File too large to diff"),
    };

    let num_width = wrap::num_width(&diff);
    let fade = fade_amount(&diff);
    let wrap_width = app.wrap.then_some(inner.width as usize);
    // Start partway into a line when the top row is a wrapped continuation.
    let (first, mut skip) = app.director.line_at_row(app.scroll.round() as usize);
    let mut row = 0;
    'lines: for line in lines.iter().skip(first) {
        for rendered in render_line(line, num_width, fade, wrap_width).into_iter().skip(skip) {
            if row >= inner.height {
                break 'lines;
            }
            let rect = Rect {
                y: inner.y + row,
                height: 1,
                ..inner
            };
            frame.render_widget(rendered, rect);
            row += 1;
        }
        skip = 0;
    }
}

fn placeholder(frame: &mut Frame, area: Rect, msg: &str) {
    frame.render_widget(
        Paragraph::new(msg).style(Style::new().fg(palette::DIM)).centered(),
        area,
    );
}

/// 1.0 right after the change, easing to 0.0 over `FRESH_FADE`.
fn fade_amount(diff: &FileDiff) -> f32 {
    let t = (diff.at.elapsed().as_secs_f32() / FRESH_FADE.as_secs_f32()).min(1.0);
    (1.0 - t) * (1.0 - t)
}

/// Renders a diff line as one screen row, or as several when `wrap_width`
/// is set and the line is longer than the area.
fn render_line(
    line: &DiffLine,
    num_width: usize,
    fade: f32,
    wrap_width: Option<usize>,
) -> Vec<Line<'static>> {
    let text = wrap::display_text(line);
    let pieces: Vec<String> = match wrap_width {
        Some(width) => wrap::chunks(&text, wrap::text_width(line.kind, width, num_width))
            .into_iter()
            .map(str::to_string)
            .collect(),
        None => vec![text],
    };
    pieces
        .into_iter()
        .enumerate()
        .map(|(i, piece)| render_row(line, num_width, fade, piece, i > 0))
        .collect()
}

/// One screen row of a diff line. Continuation rows of a wrapped line show
/// `↪` instead of line numbers and the sign.
fn render_row(
    line: &DiffLine,
    num_width: usize,
    fade: f32,
    text: String,
    continuation: bool,
) -> Line<'static> {
    let num = |n: Option<usize>| match n {
        Some(n) if !continuation => format!("{n:>num_width$}"),
        _ => " ".repeat(num_width),
    };
    let gutter_style = Style::new().fg(palette::DIM);
    let marker = if line.fresh {
        Span::styled("▎", Style::new().fg(palette::FRESH_MARK))
    } else {
        Span::raw(" ")
    };

    let (sign, fg, bg) = match line.kind {
        LineKind::HunkHeader => {
            return Line::from(vec![
                Span::raw(" "),
                Span::styled(text, Style::new().fg(palette::HUNK).add_modifier(Modifier::DIM)),
            ]);
        }
        LineKind::Context => (" ", None, None),
        LineKind::Added => (
            "+",
            Some(palette::ADD_FG),
            Some(highlight(palette::ADD_BG, palette::ADD_FRESH, line.fresh, fade)),
        ),
        LineKind::Removed => (
            "-",
            Some(palette::DEL_FG),
            Some(highlight(palette::DEL_BG, palette::DEL_FRESH, line.fresh, fade)),
        ),
    };
    let mut style = Style::new();
    if let Some(fg) = fg {
        style = style.fg(fg);
    }
    if let Some(bg) = bg {
        style = style.bg(bg);
    }
    let sign = if continuation {
        Span::styled("↪ ", style.patch(gutter_style))
    } else {
        Span::styled(format!("{sign} "), style)
    };
    Line::from(vec![
        marker,
        Span::styled(format!("{} {} ", num(line.old_no), num(line.new_no)), gutter_style),
        sign,
        Span::styled(text, style),
    ])
    .style(bg.map_or_else(Style::new, |bg| Style::new().bg(bg)))
}

fn highlight(base: Color, fresh: Color, is_fresh: bool, fade: f32) -> Color {
    if !is_fresh || fade <= 0.0 {
        return base;
    }
    let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (base, fresh) else {
        return base;
    };
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * fade).round() as u8;
    Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
}

/// "4s", "1.5s".
fn format_secs(d: std::time::Duration) -> String {
    let secs = d.as_secs_f32();
    if secs.fract() == 0.0 {
        format!("{secs:.0}s")
    } else {
        format!("{secs:.1}s")
    }
}

fn status_char(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "A",
        FileStatus::Modified => "M",
        FileStatus::Deleted => "D",
    }
}

fn status_style(status: FileStatus) -> Style {
    let color = match status {
        FileStatus::Added => palette::ADD_FG,
        FileStatus::Modified => palette::ACCENT,
        FileStatus::Deleted => palette::DEL_FG,
    };
    Style::new().fg(color).bold()
}

/// Shortens a path from the left so the file name stays visible.
fn truncate_left(path: &str, width: usize) -> String {
    let len = path.chars().count();
    if len <= width {
        return format!("{path:<width$}");
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let tail: String = path.chars().skip(len - (width - 1)).collect();
    format!("…{tail}")
}
