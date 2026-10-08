mod app;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use diff_live_core::{BaseMode, Session};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

/// Live, auto-following diff viewer for watching coding agents work.
#[derive(Parser)]
#[command(name = "diff-live", version)]
struct Args {
    /// Repository (or any path inside it) to watch.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// What to diff against.
    #[arg(long, value_enum, default_value_t = Base::Session)]
    base: Base,

    /// Seconds to show each file when cycling (adjust live with +/-).
    #[arg(long, value_name = "SECONDS", default_value_t = 4.0, value_parser = parse_cycle)]
    cycle: f32,
}

fn parse_cycle(s: &str) -> Result<f32, String> {
    let secs: f32 = s.parse().map_err(|_| format!("`{s}` isn't a number"))?;
    if (0.5..=600.0).contains(&secs) {
        Ok(secs)
    } else {
        Err("must be between 0.5 and 600 seconds".into())
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Base {
    /// The working tree when diff-live started (survives mid-session commits).
    Session,
    /// The HEAD commit when diff-live started, including pre-existing changes.
    Head,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mode = match args.base {
        Base::Session => BaseMode::SessionStart,
        Base::Head => BaseMode::Head,
    };
    let session = Session::start(&args.path, mode)?;

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    // ratatui's panic hook restores the terminal; release the mouse too.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));

    let result = app::App::new(session, Duration::from_secs_f32(args.cycle)).run(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}
